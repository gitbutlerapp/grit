---
title: Benchmarks
summary: Grit vs Git on realistic workloads, from the committed grit-bench baseline.
---

Performance work is measured on the factory VM and in CI against the **system** `git` binary. Micro-benchmarks live in `grit-lib` (Criterion) and command-level scenarios in `grit-bench` (`grit-utils`).

For a map of **grit-lib** modules, **grit** commands, common workflows, and every **`grit-bench daily`** scenario id, see [`grit-utils/MATRIX.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/MATRIX.md). The same ids appear with **time and memory ratios** on the [Performance overview (daily matrix)](performance-overview.md) page.

Command-level numbers below come from the committed **grit-bench** JSON baselines listed in `content/docs/site.toml`. Each scenario runs **hyperfine** against the system **`git`** binary and a release **`grit`** build on the same machine; times are wall-clock milliseconds (mean and spread from timed runs).

## grit-bench fixture cache

Scenario suites reuse prepared repositories under **`GRIT_BENCH_CACHE`** (default `$XDG_CACHE_HOME/grit-bench`). **`grit-bench fixtures prepare`** clones upstream **git.git** at tag **`v2.47.0`** (bare, verified `HEAD` oid) and generates synthetic tiers **S**, **M**, and **L** with the grit-lib generator. **`grit-bench fixtures list`** prints identity (name, tier, `HEAD`, object/ref/file counts). Mutating runs take a **`git clone --local`** working copy so the cache stays read-only. JSON reports may include a top-level **`fixtures`** array with the same identity fields.

```bash
./target/release/grit-bench fixtures prepare --format json
./target/release/grit-bench fixtures list --format markdown
```

### Daily matrix: partial staging on `large.txt`

Two **`grit-bench daily`** worktree scenarios share the same prepare hook (`prepare-worktree-add-hunks`): reset index/worktree to **`HEAD`**, then apply **500** one-line edits on a committed **50 000**-line `large.txt`. Each timed iteration runs that prepare again so partial staging from the previous sample does not carry over.

| Scenario id | Grit (timed) | Git (timed) |
| --- | --- | --- |
| **`add/hunks@large-file`** | `grit add --hunks` with the first **half** of unstaged hunk ids (same selection grit uses when writing bench artifacts) | `git diff -- large.txt`, keep the first **half** of unified hunks in file order, pipe to **`git apply --cached -`** |
| **`add/patch-file@large-file`** | `grit add --patch-file` on the prebuilt half-hunk unified diff written during prepare | **`git apply --cached`** on the same prebuilt patch (apply-only) |

Acceptance for both: grit median wall time and peak RSS must be **≤** git on the same scenario.

## grit-lib: parallel object hashing

Criterion group `hash/batch_parallel` (`cargo bench -p grit-lib --bench objects hash/batch_parallel`) hashes **20 000** Git blob objects of **16 KiB** each using [`hash_objects_parallel`](rustdoc:grit_lib::objects::hash::hash_objects_parallel) (SHA-1, release build).

| Threads | Time (median) | Throughput |
| ------- | ------------- | ---------- |
| 1       | 173 ms        | 1.76 GiB/s |
| 8       | 44.5 ms       | 6.86 GiB/s |

**Speedup (8 vs 1 thread): ~3.9×** on the factory VM (2026-10-07).

Parallel hashing falls back to a serial loop when there are fewer than [`PAR_HASH_MIN_ITEMS`](rustdoc:grit_lib::objects::hash::PAR_HASH_MIN_ITEMS) (32) objects or less than [`PAR_HASH_MIN_TOTAL_BYTES`](rustdoc:grit_lib::objects::hash::PAR_HASH_MIN_TOTAL_BYTES) (256 KiB) of payload, so small batches avoid thread overhead.

## grit-lib: OID hash maps

Criterion bench **`oid_hashmap`** (`cargo bench -p grit-lib --bench oid_hashmap`) compares `std` `HashMap`/`HashSet` keyed by [`ObjectId`](rustdoc:grit_lib::objects::ObjectId) (SipHash over the full digest) against [`OidHashMap`](rustdoc:grit_lib::objects::OidHashMap) / [`OidHashSet`](rustdoc:grit_lib::objects::OidHashSet) (hashbrown tables with [`OidBuildHasher`](rustdoc:grit_lib::objects::OidBuildHasher): first eight digest bytes; [`finish`](rustdoc:grit_lib::objects::OidHasher::finish) XORs that prefix with a process-wide random value). Insert has two symmetric variants at **1 M** keys (SHA-1 OIDs from seeded `ObjectHasher`, distinct first-eight-byte prefixes): **`oid_hashmap_insert_presized`** (both sides call `with_capacity`) and **`oid_hashmap_insert_default_growth`** (both start from empty maps). Factory VM medians from `cargo bench -p grit-lib --bench oid_hashmap -- --sample-size 10 --warm-up-time 1 --measurement-time 3 oid_hashmap_insert` (2026-10-21): presized **~72 ms** SipHash vs **~49 ms** OID (~**1.48×**); default-growth **~115 ms** vs **~72 ms** (~**1.59×**). Lookup and contains groups use the same pre-built tables on both sides. Hot paths (rev-list commit cache, pack delta planning, fsck connectivity, ODB session caches, and related maps) call [`oid_hash_map_with_capacity`](rustdoc:grit_lib::objects::oid_hash_map_with_capacity) / [`oid_hash_set_with_capacity`](rustdoc:grit_lib::objects::oid_hash_set_with_capacity) when an object count is known; public APIs still expose standard `HashSet`/`HashMap` at boundaries.

## grit-lib: ODB backend (pluggable object store)

Criterion group **`odb_backend`** compares post-refactor [`Odb`](rustdoc:grit_lib::objects::odb::Odb) against the saved baseline **`odb-before`** (`cargo bench -p grit-lib --bench odb_backend -- --baseline odb-before`). Built-in layouts route hot reads through [`FilesSource`](rustdoc:grit_lib::objects::odb::store::FilesSource) on the primary store and [`CompositeStore`](rustdoc:grit_lib::objects::odb::store::CompositeStore) for alternates so behaviour matches the pre-refactor oracle tests while keeping a pluggable [`ObjectStore`](rustdoc:grit_lib::objects::odb::ObjectStore) boundary.

**Pluggable object store overhead (factory VM, 2026-10-09):** Re-run Criterion with `--baseline odb-before` after ODB changes; command-level **`grit-bench odb-backend`** baselines are in **`odb-backend-before.json`** (pre-refactor capture, scenario ids suffixed `-pre-refactor`) and **`odb-backend-after.json`**. Post-refactor Grit medians for cat-file batch/check are much lower than the pre-refactor capture on the same fixture; rev-list wall time is similar while Grit/Git **ratios** can drift when system `git` speeds up between runs.

Criterion measures [`Odb::read`](rustdoc:grit_lib::objects::odb::Odb), [`read_info`](rustdoc:grit_lib::objects::odb::Odb::read_info), [`exists`](rustdoc:grit_lib::objects::odb::Odb::exists) (hit and miss), and [`write`](rustdoc:grit_lib::objects::odb::Odb::write) on four deterministic layouts built in `grit-lib/benches/fixture.rs`:

| Fixture | Shape |
| --- | --- |
| `loose_10k` | 10 000 loose blobs, no packs |
| `pack_100k` | Single repacked pack (~100 000 objects) |
| `midx_8` | Eight pack layers with multi-pack-index (`git prune-packed` so hits are pack-only, not loose) |
| `alternate_only` | Primary store empty; objects only in an alternate |

An additional benchmark, **`odb_backend/exists_miss_loop_10k`**, calls `exists` on 10 000 distinct missing OIDs against the 100k pack fixture (models add/status miss scans).

## grit-lib: abbreviating ids on custom object stores

Criterion bench **`abbreviate`** (`cargo bench -p grit-lib --bench abbreviate`) calls [`abbreviate_object_id`](rustdoc:grit_lib::revwalk::rev_parse::abbreviate_object_id) with a 7-digit minimum for 200 ids out of 200,000 blobs held in three stores: a [`MemoryStore`](rustdoc:grit_lib::objects::odb::store::MemoryStore), a store that implements only the required [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) methods (so it uses the default full-scan `lookup_prefix`), and one pack in a files ODB. Factory VM medians (2026-10-24, issue #1736):

| Store | Before | After |
| --- | --- | --- |
| `MemoryStore` | 3.89 s | 168 ms |
| Required methods only | 3.90 s | 229 ms |
| Files ODB, one pack | 668 µs | 601 µs |

Prefix matching now compares raw id bytes against a prefix parsed once ([`ObjectIdPrefix`](rustdoc:grit_lib::objects::ObjectIdPrefix)) instead of formatting every candidate as hex, and abbreviation makes one lookup per id instead of one per candidate length. A store without an index still scans every id once per lookup; override `lookup_prefix` with a range scan to avoid that (see [Object stores](../library/object-stores/#short-ids-and-prefix-lookup)).

## grit-lib: pack write (100k reuse vs fresh)

Criterion group **`pack`** (`cargo bench -p grit-lib --bench pack_write`) rebuilds the full closure of the repacked **`pack_100k`** fixture (~100 000 objects) via [`build_pack_with_parallel_threads`](rustdoc:grit_lib::objects::pack_objects::build_pack_with_parallel_threads):

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

Criterion group **`odb_read_parallel/packed_blobs`** (`cargo bench -p grit-lib --bench odb_read_parallel`) measures throughput when worker threads share one [`Odb`](rustdoc:grit_lib::objects::odb::Odb) and hold [`Odb::with_pack_read_context`](rustdoc:grit_lib::objects::odb::Odb::with_pack_read_context) for the worker lifetime. Each iteration reads a partition of **50 000** whole-object pack blobs (**3072-byte** payload; loose copies are removed after packing so hits are pack-only). Baseline **`odb-read-parallel.json`** records Criterion central estimates on the factory VM (**4 logical CPUs**, Rust **1.99** stable).

| Threads | Median time (50k reads) | Throughput |
| ------- | ----------------------: | ---------- |
| 1 | ~149 ms | ~336 Kelem/s |
| 4 | ~51 ms | ~983 Kelem/s |

**Speedup (4 vs 1 thread): ~2.9×** on the factory VM (2026-10-11). This exercises the explicit batch read-context API and shared [`PackStore`](rustdoc:grit_lib::objects::pack_store::PackStore) catalog, not ad-hoc per-read context installation.

```bash
cargo bench -p grit-lib --bench odb_read_parallel -- packed_blobs
```

## grit-bench: reachability bitmaps (`git.git`)

Scenario group **`bitmaps`** measures three acceptance metrics on upstream **`git/git`**: commit counting, object counting with bitmap index, and stateless **`upload-pack`** clone serving. A fourth scenario, **`serve-clone-no-bitmaps-git.git`**, serves the same clone from a copy of the fixture with the `.bitmap` removed (one Git-written pack with its `.rev` reverse index, `ensure_git_git_no_bitmaps`), which is how most servers that never wrote bitmaps look: objects are enumerated by walking history and the pack reuses the on-disk deltas. **`serve-clone-blobless-git.git`** and **`serve-clone-blobless-no-bitmaps-git.git`** send the same request with `filter blob:none` (the server side of a blobless partial clone, `drive serve-blobless`), with `uploadpack.allowFilter` enabled on both fixtures. The fixture is a bare **single-branch tag clone** at **`v2.47.0`** (override with **`GRIT_BENCH_GIT_GIT_TAG`**) under **`GRIT_BENCH_ODB_CACHE`**, prepared once with **Git-written** pack bitmaps (`git repack -adb`, `git commit-graph write --reachable`) and separately with **grit-written** pack bitmaps (`git repack -ad`, commit-graph, then `PackBitmapWriter` — see `ensure_bitmaps_grit_repo` in `grit-utils/src/bitmap_fixture.rs`). Library workloads use hidden **`grit-bench drive`** commands (`rev-list-count`, `rev-list-count-objects`, `serve-clone`); Git runs the equivalent **`git rev-list`** / **`git upload-pack`** invocations with the same isolated config as other grit-bench suites.

**Methodology.** Hyperfine warmups and minimum runs match other factory captures (see each JSON `machine` block). Ratios are **Grit median ÷ Git median**. **Peak RSS** is recorded per side via `grit-bench measure-rss` / hyperfine wrappers. **`serve-clone`** runs under **`GRIT_BENCH_SERVE_TIMEOUT_SECS`** (default **120**) and **`GRIT_BENCH_SERVE_MAX_RSS_GIB`** (default **12**). Reproducibility: run the suite twice back-to-back on the same fixture; medians should agree within **±10% relative** (`grit-bench compare … --tolerance 0.10` uses an absolute ratio tolerance — interpret large ratios accordingly).

```bash
cargo build --release -p grit-utils -p grit-cli
./target/release/grit-bench bitmaps --format json --output grit-utils/baselines/bitmaps-after.json
# Optional grit-written pack bitmap sidecar (separate JSON; default fixture stays Git-prepared):
GRIT_BENCH_BITMAP_FIXTURE=grit ./target/release/grit-bench bitmaps --format json --output grit-utils/baselines/bitmaps-grit-written.json
```

**Before (`bitmaps-before.json`, 2026-10-10, pre-reader/writer stack):** bitmap reader missing — object count ~**23 000×** vs Git; serve-clone hit **memory_cap**.

**First acceptance run (factory VM 2026-10-16, release `grit` on the pack-reuse + bitmap stack):** `rev-list-count` **18.0×**, `rev-list-count-objects` **63.2×**, and `serve-clone-git.git` hit the **120 s** cap (`exit 124`). Two back-to-back runs agreed within **~6–8% relative** on Grit medians for the rev-list scenarios.

**Current (`bitmaps-after.json`, 4-vCPU Xeon VM 2026-10-25, Git 2.43.0, after the filtered-serve fix for [#1819](https://github.com/gitbutlerapp/grit/issues/1819)):**

| Scenario | Git median | Grit median | Ratio | Git peak RSS | Grit peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| `rev-list-count-git.git` | 56.5 ms | 686.0 ms | **12.1×** | ~29 MiB | ~79 MiB |
| `rev-list-count-objects-git.git` | 16.9 ms | 719.7 ms | **42.7×** | ~23 MiB | ~79 MiB |
| `serve-clone-git.git` | 654.4 ms | 4235.8 ms | **6.5×** | ~278 MiB | ~765 MiB |
| `serve-clone-blobless-git.git` | 296.1 ms | 2146.1 ms | **7.2×** | ~156 MiB | ~364 MiB |
| `serve-clone-no-bitmaps-git.git` | 3920.3 ms | 12079.7 ms | **3.1×** | ~462 MiB | ~811 MiB |
| `serve-clone-blobless-no-bitmaps-git.git` | 2810.2 ms | 8817.5 ms | **3.1×** | ~325 MiB | ~446 MiB |

Serving used to load every object of the clone into memory (blobs included) and decode the pack's `.rev` file again for every `OFS_DELTA` base, so both serve scenarios ran past the cap. Upload-pack now copies packed objects and deltas as they are, delta-searches only blobs that are not already deltas, and streams the pack. On `rust-lang/cargo` (195k objects, 80 MB pack, no bitmap) the server side of a full clone went from **91.3 s / 3.6 GB** peak RSS to **3.0 s / 420 MB** (Git **2.5 s / 161 MB**).

**Filtered serving.** Without a usable bitmap, a `filter` request used to go through a separate `rev-list` walk that read every reachable tree once per commit. The pack enumeration walk now applies the filter itself, so a filtered pack costs what an unfiltered one does minus the objects it leaves out. Server side of `blob:none` requests on the same VM (single runs; the bitmap row is a hyperfine mean of 10):

| Request | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| `git.git` every ref, no bitmap | 2.9 s / 316 MB | 277.9 s / 11.7 GB | 8.6 s / 451 MB |
| `git.git` every ref, bitmap | 0.33 s / 143 MB | 2.18 s / 367 MB | 2.19 s / 367 MB |
| `rust-lang/cargo` clone, no bitmap | 1.4–1.7 s / 148 MB | 55.7 s / 1.8 GB, then failed | 1.4–1.5 s / 260 MB |
| `rust-lang/cargo` fetch of tag `0.58.0` | 0.53 s / 69 MB | 5.8 s, then failed | 0.65 s / 132 MB |
| `rust-lang/cargo` fetch of 400 new commits | 0.16 s / 29 MB | failed | 1.65 s / 161 MB |

The cargo clone and tag fetch used to fail because two annotated tags on one commit (`0.58`, `0.58.0`) kept only one tag object in the pack, and every filtered fetch with haves failed. With a bitmap, the bitmap already selected the objects; the remaining gap there is the same planning and writing cost as the unfiltered `serve-clone-git.git`.

**Grit-written bitmap fixture:** set **`GRIT_BENCH_BITMAP_FIXTURE=grit`** when running `grit-bench bitmaps` to prepare the same pinned **`git.git`** clone with **`git repack -ad`**, commit-graph, and a grit-lib **`PackBitmapWriter`** sidecar (`ensure_bitmaps_grit_repo`). The committed **`bitmaps-after.json`** uses the **Git-prepared** fixture (default); capture a separate JSON for grit-written bitmaps when comparing interoperability (see factory notes for the grit-fixture run ratios).

**Acceptance bar (roadmap item 14):** each ratio **≤ 1.2×** (and serve-clone within the same bar). The after run **passes compatibility** (bitmaps read/write, serve packs validate with `git index-pack --strict` after the thin-pack reuse fix) but **misses the 1.2× performance bar** on all three command-level metrics on this tag-heavy **`git.git`** fixture (~954 tag refs). Follow-ups (not retargeting the bar):

- **Multi-tip bitmap counting:** optimize `rev-list --count --all` / `--objects --use-bitmap-index` when hundreds of tag tips are present (Git ~60 ms vs grit ~1 s on the same fixture).
- **Serve-clone wall time and memory:** both serve scenarios now finish well under the cap. What remains is the history walk that enumerates objects without a bitmap (about 3× Git's on `git.git`, dominated by visited-set probes and tree reads), bitmap-path planning, and memory held by per-object maps during delta-reuse planning.
- **Re-run `bitmaps-after.json`** after those land; keep **`bitmaps-before.json`** for regression context.

Intermediate capture on the same VM before the final writer stack (**2026-10-10**) remains in **`bitmaps-before.json`** for the generated tables (`rev-list-count` **~3.3×**, object count unusable, serve **memory_cap**).

## grit-lib: parallel index-pack (in-memory)

Release [`index_pack_bench`](https://github.com/gitbutlerapp/grit/tree/main/grit-lib/examples/index_pack_bench.rs) on a **~112 500-object** depth-50 pack (`git fast-import` + `git repack -adf --depth=50`, factory VM 2026-10-12). Grit uses [`pack_index_records_with_threads`](rustdoc:grit_lib::objects::unpack_objects::pack_index_records_with_threads) (`GRIT_INDEX_PACK_BENCH_PACK` / `GRIT_INDEX_PACK_THREADS`); timings cover in-memory record building only (no `.idx` write). System **`git index-pack`** rows are end-to-end on a fresh temp directory each run (writes `.idx`).

| Command | Median (5 runs) |
| ------- | ---------------- |
| grit index-pack (records only), 1 thread | 1.21 s |
| grit index-pack (records only), 4 threads | 0.49 s |
| `git index-pack --threads=1` (end-to-end) | 1.25 s |
| `git index-pack --threads=4` (end-to-end) | 0.43 s |

**Speedup (4 vs 1 thread, grit records): ~2.47×.** Multi-threaded builds overlap pack scanning with batched parallel inflate+hash (ordered work pool; pack order preserved in the public record list). The single-thread path scans once and inflates+hashes serially—no redundant second skip pass. Delta chains still inflate, apply, and hash in parallel rounds.

Integration coverage uses a **≥ 50 000** object fixture built with `git fast-import` and shared across tests via `OnceLock` (`grit-lib/tests/index_pack_parallel.rs`); the default release suite completes in about **41 s** on the factory VM.

## Hashing

Checksum throughput uses plain accelerated SHA-1/256 from the `sha1` / `sha2` crates ([`HashingWriter`](rustdoc:grit_lib::objects::hash::HashingWriter), Criterion `hash_sha1` / `hashing_writer_sha1`). Object-id hashing on ingestion uses [`ObjectHasher::for_object`](rustdoc:grit_lib::objects::hash::ObjectHasher) with the **sha1dc** crate (collision detection on the canonical object header + payload). OpenSSL numbers are from `openssl speed -evp sha1 -bytes 16384` and `openssl speed -evp sha256` (16 KiB blocks where shown). Git blob hashing uses `git hash-object` on a 256 MiB file vs the `gritx-hash-file` example (same canonical blob object id, no object-database write).

**Environment (2026-10-21):** Intel Xeon (SHA-NI present), Linux, `git` 2.43.0, OpenSSL 3.0.13, Rust stable 1.99. `hash-info --json` reports `{"sha1":"x86_sha_ni","sha256":"x86_sha_ni"}`.

### Micro (Criterion + hyperfine)

| Workload | grit-lib | Reference | grit / reference |
| -------- | -------- | --------- | ---------------- |
| SHA-1 checksum, 1 MiB buffer | 1.59 GiB/s | OpenSSL SHA-1 @ 16 KiB: 1.88 GiB/s | **85%** |
| SHA-1 checksum via `HashingWriter`, 1 MiB | 1.93 GiB/s | OpenSSL SHA-1 @ 16 KiB: 1.88 GiB/s | **103%** |
| SHA-256, 1 MiB buffer | 1.76 GiB/s | OpenSSL SHA-256 @ 16 KiB: 1.73 GiB/s | **101%** |
| SHA-1 object id (`hash_object_sha1`), 1 MiB | 1.57 GiB/s | effective `git hash-object` throughput on 256 MiB: ~0.40 GiB/s | **~3.9× throughput** |
| SHA-1 blob, 256 MiB file (hyperfine) | 360 ms | `git hash-object`: 612 ms | **1.70× faster** |

Plain SHA-1 checksums at 1 MiB meet the project bar (≥ 95% of OpenSSL on the same CPU) on the `HashingWriter` path used for pack checksums. SHA-256 meets ≥ 90% of OpenSSL. Object-id hashing keeps collision detection (sha1dc) while staying well above stock Git’s sha1dc-backed `hash-object` throughput on large blobs (see **`hash-after.json`** `hash-object-256mib`: grit/git median ratio **0.61×**, **1.64×** faster than system git on the same 256 MiB `hash-object -w` run).

### End-to-end (`hash-after.json`, factory VM 2026-10-21)

Committed baseline **`grit-utils/baselines/hash-after.json`** (paired with **`hash-before.json`**) from `grit-bench hash`: 256 MiB `hash-object -w`, repacked 100k-object **`index-pack`** (SHAttered blobs excluded so grit and git compare fairly), and medium **`fsck`**.

| Scenario | Grit median | Git median | Grit / Git |
| -------- | ----------- | ---------- | ---------- |
| `hash-object-256mib` | 3.819 s | 6.263 s | **0.610×** (1.64× faster) |
| `index-pack-git-git` (100k repacked) | 123.1 ms | 197.7 ms | **0.623×** (1.61× faster) |
| `fsck-medium` | 416.7 ms | 239.4 ms | **1.741×** (grit slower; follow-up [#1475](https://maint.grit-scm.com/factory/1475)) |

Versus **`hash-before.json`**, grit median on `hash-object-256mib` is **−3.9%** (3971.9 ms → 3818.5 ms), within the **5%** acceptance bar. Profiling the `hash-object -w` path showed a redundant **payload-only sha1dc pass** on every large blob after canonical object-id hashing; **`grit-lib`** now skips that pass when the payload is larger than 64 KiB and does not start with a SHAttered block prefix. Loose writes use [`hash_object`](rustdoc:grit_lib::objects::hash::hash_object) for the object id (no duplicate `ObjectHasher` in the store layer). `index-pack` uses the repacked 100k fixture (see **`factory/checked-object-hash-2718`**) so before/after medians for that scenario are not directly comparable.

### Commands

```bash
# Backends selected on this CPU
cargo run -p grit-examples --bin hash-info -- --json

# Criterion throughput (64 B … 64 MiB)
cargo bench -p grit-lib --bench hash

# OpenSSL + hyperfine vs git (creates a 256 MiB temp file)
./scripts/bench-hash.sh
```

## Merge, pick, revert conflict workflows (2026-10-23)

The **`conflicts`** grit-bench group times full porcelain workflows on synthetic **1k** and **10k** file fixtures (reset to a fixed state each hyperfine iteration). Scenarios: clean **`merge --no-ff`**, **`merge` → `restore --theirs` → `add` → `--continue`** on overlapping edits, **`pick`** of twenty commits in one invocation vs `git cherry-pick base..topic`, and **`revert HEAD`** / **`revert --mainline 1`** on a recorded merge commit. In-memory **`merge --check`** stays in **`merge-check.json`** (library driver vs `git merge-tree --write-tree`).

Committed baseline: **`grit-utils/baselines/conflicts.json`**. Factory VM medians vs **`git 2.43.0`** (2026-10-23): **`merge-no-ff`**, **`revert-single`**, and **`revert-mainline`** are at or below Git time at both tiers; **`pick-range-20`** is **~1.15×** at 10k files; **`merge-conflict-continue`** and **`pick-range-20`** at 1k remain above **1.0×** (tracked as speed follow-ups). At **10k** files, **`merge-conflict-continue`** is still **~5.5×** Git — dominated by grit-side restore/add on the wide conflict set, not by merge prediction.

```bash
./target/release/grit-bench conflicts --sizes 1000,10000 --format json --output grit-utils/baselines/conflicts.json
```

## Full-history walks use the commit-graph (2026-10-24)

Listing every commit with a valid commit-graph took as long as walking without one ([#1745](https://github.com/gitbutlerapp/grit/issues/1745)). The walk took parents and dates from the graph, but still inflated every commit object to cache its subject line, and looked each commit up in the graph three times. Commits found in the graph are no longer read during the walk. Callers that print subjects read them for the listed commits only. Each commit now costs one graph lookup.

[`rev_list`](rustdoc:grit_lib::revwalk::rev_list::rev_list) on upstream `git/git` (82 563 commits, commit-graph from `git gc`), in process, release build, `origin/main` → this change, against system `git 2.43.0`:

| Walk | Git ms | Grit ms with graph, before → after | Grit ms without graph |
| --- | ---: | ---: | ---: |
| default order | 71–106 | 760–860 → **105** | 740 |
| `--topo-order` | 90–126 | 750–760 → **102** | 705 |
| `--first-parent` | 20–30 | 176 → **23** | 167 |

Git's times include process start-up. Criterion `history` groups on the 10k-commit synthetic fixture, `--baseline main`: `revwalk_default`, `revwalk_topo` and `revwalk_date_order` with a graph are **−80 to −82%**, `revwalk/log_limit` and `revwalk/limit_near_full_history` are **−81 to −89%**, and the no-graph rows are within the run-to-run drift of an A/A run on main (±4%). `grit revs HEAD` on `git/git` drops from **1.73 to 1.00 s**. Its peak RSS stays at **283 MB**, because it builds every row (full message included) before rendering. `git log` with subjects takes 0.70 s and 66 MB.

The `revwalk` suite gained `revs-list-deep-no-graph` and `revs-list-deep-commit-graph` (every commit id from `HEAD`, library walk, vs `git rev-list HEAD`) in **`revwalk-queries.json`**:

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench revwalk --format json --output grit-utils/baselines/revwalk-queries.json
cargo bench -p grit-lib --bench history -- 'revwalk_(default|topo|date_order)/'
```

## Clean-tree status (2026-10-24)

Dogfooding on Windows (issue #1702) found `grit status` re-reading tracked files on clean trees and running 3.5–5× slower than Git on rust-lang/cargo and git.git. Four causes, all in `grit-lib`:

- After a stat match, the content-hash skip refused to trust the index when `core.filemode=false` (the Windows default) or when the cached size was zero. Serial scans (fewer than 1000 tracked files, `--threads 1`, `core.preloadIndex=false`) therefore re-hashed every file. Zero size is now trusted only for the empty blob, as in Git.
- `status` never rewrote an index whose entries were racily clean (index written in the same second as the files, as after `git clone` or `git commit`), so every run re-hashed them. It now persists the index once, as Git does, and hashes each racy file only once.
- The untracked walk paid three path probes per directory (`is_dir`, `.git`, `.gitignore`), and the symlinked-ancestor check ran as a serial pre-pass before the parallel stat workers. On Windows each path stat opens a handle. Probes now come from the directory listing or run inside the workers, and Windows uses a single attribute query for them.
- Outside Windows, tracked files are stat-ed by path like Git's preload instead of listing their directories: ext4 hashed `readdir` dominated large directories.

Same Linux VM sitting, release builds, system `git 2.43.0`, clean trees with `GIT_CONFIG_GLOBAL` empty. `git status --porcelain` → `grit status --json`, median ms over 25 runs; grit is `origin/main` before → after:

| Repository | Tracked files | Git | Grit before → after | Tracked part (`-uno` / `--untracked no`): Git, Grit before → after | Peak RSS Git, Grit before → after |
| --- | ---: | ---: | ---: | ---: | ---: |
| rust-lang/cargo | 3,078 | 10.5 | 23.5 → **17.3** | 3.2, 8.2 → **6.4** | 6, 15 → 15 MiB |
| git/git | 4,857 | 6.6 | 23.0 → **9.6** | 3.3, 18.7 → **5.2** | 6, 16 → 15 MiB |
| synthetic depth-7 tree | 48,750 | 54.9 | 59.7 → **48.2** | 18.0, 33.3 → **26.6** | 17, 23 → 23 MiB |
| 60 × 4 MB binaries, `core.filemode=false` | 60 | 1.3 | 358.1 → **2.4** | 1.3, 353.1 → **2.2** | 4, 20 → 13 MiB |
| 200 × 1 MB text, `core.filemode=false` | 200 | 1.3 | 377.0 → **2.6** | 1.3, 381.3 → **2.4** | 4, 14 → 13 MiB |

`grit-bench status` (synthetic 1k/10k/50k, committed as `status-clean-tree-before.json` → `status-clean-tree-after.json`): `status-clean-1000` 7.0 → 3.9 ms and `status-dirty-1000` 6.9 → 4.1 ms; the 10k rows no longer alternate with a ~33 ms racy re-hash mode; 50k rows are unchanged within noise.

```bash
./target/release/grit-bench --format json --output grit-utils/baselines/status-clean-tree-after.json status --sizes 1000,10000,50000
```

## Delete-heavy switch and hard reset (2026-10-24)

On Windows with real-time antivirus, deleting about 2000 files with `grit switch` or `grit reset --hard` took several seconds where Git took half a second (issue #1754). The cost was per-file work that opens files, which antivirus scanning makes expensive, rather than the deletes themselves:

- **`reset --hard` diffed the whole worktree for nothing.** Planning a hard reset ran a full index-to-worktree diff to list the edits it would discard, then threw the list away unless `--dry-run` was set. That diff stat'ed every tracked file and read every racily clean one, including the files about to be deleted. It now runs only for dry runs.
- **`reset --hard` stat'ed each file about three times.** Paths leaving the index were stat'ed and removed one by one, and every unchanged entry went through a per-path up-to-date check that stat'ed it, possibly hashed it, and stat'ed it again. Now tree-diff deletions go through the tree-diff checkout (whole directories removed at once), unchanged entries get one directory-batched stat pass, and only stat-dirty or racily clean entries are content-checked, in parallel. The scan only decides: rows written back to the index keep the stat data a path stat recorded, so directory-enumeration stats (which can differ from `lstat` on NTFS) never land in the index, and a directory that replaced a tracked file is never mistaken for it.
- **`switch` hashed racily clean files one at a time.** Right after a checkout, the files it wrote share the index's timestamp second, so switching away has to compare their content (Git does the same). The guard re-stat'ed each one and hashed them serially. It now reuses the directory scan's stat and hashes them on the index-refresh thread pool.

The hard-reset rework also fixed a correctness hole: raciness of unchanged entries was judged against the index written halfway through the reset, so a same-size edit made within the index's timestamp second could survive `reset --hard` and be recorded as clean.

Syscalls on the issue's synthetic tree (480 directories of 100 files, the target commit deletes 20 directories), run straight after the reverse checkout so the deleted files are racily clean, counted with `strace -f`:

| Command | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| `reset --hard`: stat calls | 46 133 | 146 883 | **46 341** |
| `reset --hard`: files opened | 0 | 2 000 | **0** |
| `switch`: stat calls | 54 650 | 6 342 | **4 347** |
| `switch`: racy files read | 2 000 | 2 000 (1 thread) | 2 000 (pool) |

`grit-bench daily` rows on the same shape (the fixture is packed, each run goes to the delete-heavy commit and back), same VM sitting, system `git 2.43.0`, committed in **`daily-delete-heavy-before.json`** and **`daily-delete-heavy-after.json`**:

| Scenario | Git ms (before / after run) | Grit ms before → after | Grit ÷ Git before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: | ---: |
| `reset/hard-delete-heavy@wide-48k` | 305.3 / 306.0 | 557.3 → **276.0** | 1.83× → **0.90×** | 62 → **49** MB |
| `switch/delete-heavy@wide-48k` | 180.7 / 179.6 | 145.4 → 138.1 | 0.80× → 0.77× | 32 → 33 MB |

On Linux, opening a file is cheap, so the switch row only moves within noise. The win there is on platforms where each open is slow, and it can't be timed on this VM.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench daily --only switch/delete-heavy@wide-48k,reset/hard-delete-heavy@wide-48k \
  --min-runs 10 --output grit-utils/baselines/daily-delete-heavy-after.json
```

## Pick keeps the cache-tree (2026-10-24)

The daily census flagged `pick/one@synth-100k` going from 0.95× to 1.22× Git. The replay sequencer had started loading the index without its cache-tree extension to save memory. Without a cache-tree, the check that the index matches `HEAD` flattens the whole `HEAD` tree into a map, which costs more time and more memory than the extension it skipped. The index was also parsed a second time for the first replay step, and the index written after the pick lost its cache-tree. Pick and revert now keep the cache-tree, and the snapshot from the cleanliness check feeds the first step.

Same VM sitting, release builds, system `git 2.43.0`, `origin/main` before → after:

| Scenario | Git ms | Grit ms before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: |
| `pick/one@synth-100k` (daily) | 208.1 | 245.8 → **180.5** | 63.5 → **47.6** MiB |
| `pick-100000` | 204.0 | 241.4 → **154.5** | 65.3 → **49.4** MiB |
| `revert-100000` | 195.9 | 241.5 → **155.9** | 66.1 → **51.1** MiB |
| `pick-series-100000` | 1014.5 | 3822.5 → **2057.9** | 67.0 → **44.2** MiB |
| `pick-10000` | 140.5 | 75.2 → **73.4** | 29.2 → **26.1** MiB |
| `revert-10000` | 136.1 | 81.0 → **72.2** | 29.6 → **26.9** MiB |
| `pick-series-10000` | 137.6 | 495.3 → **288.2** | 25.8 → **22.9** MiB |
| `pick-range-20` | 135.3 | 116.7 → **113.5** | 24.2 → **22.3** MiB |

`switch-*` and `merge-*` rows are unchanged within noise. `hot-paths-after.json` is refreshed from the after run.

## Pick, revert and merge apply path (2026-10-22)

A clean `grit pick` / `grit revert` of a commit touching 2000 paths spent most of its time on work that grows with the number of touched paths, not on I/O:

- **Sparse tree merge.** "Is anything under this directory touched?" and "does the other side have a file under this path?" scanned every touched path, once per tree entry visited and once per file in the directory/file pass. A fallback re-walked each side tree from the root, cloning parsed trees along the way. A per-merge prefix index makes these hash lookups. Parsed trees are now shared, not copied. The merged tree is identical.
- **Index after the merge checkout.** Checkout wrote the index, with fresh stat data for every file it wrote. Folding in the merge result then removed and re-inserted every touched row (one O(n) vector drain each) and re-stat'ed every file. A second index write re-hashed every same-second entry as racily clean. Rows checkout already left identical are now kept, and the index is written once, with checkout's racy policy. `post-index-change` runs once.
- **Clean-worktree guard.** The dense index-to-worktree pass lstat'ed every tracked file twice. It now trusts the parallel scan's stat for unchanged, non-racy entries.

`grit-bench hot-paths` (default sizes, warmup and runs), Grit before → after on the same VM sitting, vs system `git 2.43.0`. The refreshed numbers are committed in **`hot-paths-after.json`**:

| Scenario | Git ms | Grit ms before → after | Grit ÷ Git before → after |
| --- | ---: | ---: | ---: |
| `pick-10000` | 110.3 | 203.5 → **58.4** | 1.80× → **0.53×** |
| `revert-10000` | 105.7 | 239.2 → **60.2** | 2.25× → **0.57×** |
| `merge-10000` | 18.6 | 28.5 → **22.6** | 1.53× → **1.21×** |
| `pick-series-10000` | 111.0 | 512.4 → **300.7** | 4.60× → **2.71×** |
| `pick-100000` | 185.0 | 1802.3 → **186.6** | 9.33× → **1.01×** |
| `revert-100000` | 174.6 | 1765.7 → **185.0** | 9.37× → **1.06×** |
| `merge-100000` | 138.9 | 138.4 → **107.1** | 0.83× → **0.77×** |
| `pick-series-100000` | 868.2 | 4985.0 → **2451.9** | 5.78× → **2.82×** |

`switch-*` rows are unchanged within noise. Peak RSS on `pick-10000` stays around **26 MiB**, against Git's 9 MiB. About 10 MiB of that is resident `grit` binary text, and the heap peak is one transient copy of the index during checkout. Memory is not addressed by this change.

```bash
./target/release/grit-bench hot-paths --sizes 10000,100000 --format json --output grit-utils/baselines/hot-paths-after.json
```

## Tags on many refs (2026-10-24)

On the many-refs fixture (11 668 packed refs, 3 333 of them tags, half annotated with loose tag objects), `grit tag` spent almost all of its time classifying tags. Every tag ref cost an object-header read, and every annotated tag cost a second header read plus a full read of the tag object to peel it. That meant thousands of loose-object inflates, whose zlib setup alone was a fifth of the run.

- **Listing from peel lines.** `packed-refs` written with the `peeled` or `fully-peeled` trait already records each annotated tag's peeled object, which is how Git avoids those reads. [`list_tags_with_detail`](rustdoc:grit_lib::porcelain::tag::list_tags_with_detail) uses [`TagDetail::Names`](rustdoc:grit_lib::porcelain::tag::TagDetail::Names) for the human names-only path and [`TagDetail::Kind`](rustdoc:grit_lib::porcelain::tag::TagDetail::Kind) for classification from peel records. Loose overrides and files without a peel trait fall back to objects. [`TagDetail::Annotation`](rustdoc:grit_lib::porcelain::tag::TagDetail::Annotation), used by `--json` and `--markdown`, still reads tag objects and preserves their documented fields.
- **Creating through the repository's ref store.** `create_tag` checked for an existing tag, and wrote the new one, through helpers that each opened a fresh ref store and so re-read and re-indexed `packed-refs`. The existence check, the write and the CLI's `HEAD` lookup now share the repository's store and its cached view. Sorted `packed-refs` files are binary-searched in place, without a line index.

`grit-bench daily` rows (20 runs per side, release build), current main before → after in one sitting against system `git 2.43.0`, committed in **`daily-tag-many-refs-before.json`** and **`daily-tag-many-refs-after.json`**:

| Scenario | Git ms | Grit ms before → after | Grit ÷ Git before → after |
| --- | ---: | ---: | ---: |
| `tag/list@many-refs` (human names) | 3.28 | 13.25 → **5.44** | 4.03× → **1.66×** |
| `tag/create@many-refs` | 2.21 | 4.36 → **4.35** | 1.97× → **1.97×** |
| `branch/create@many-refs` | 2.21 | 4.35 → **4.35** | 1.97× → **1.96×** |

The annotation-rich JSON path is not slower: hyperfine (20 runs) measured **11.6 → 10.6 ms**. Branch create is unchanged within noise, guarding the shared packed-ref path. Peak RSS for human tag listing fell from **21.1 to 15.3 MiB** in the daily rows; create remained within noise. The remaining gap to Git is mostly process start-up and CLI setup, tracked under the binary-diet follow-up in **Methodology** below.

```bash
cargo build --release -p grit-cli -p grit-utils
for id in tag/list@many-refs tag/create@many-refs branch/create@many-refs; do
  ./target/release/grit-bench daily --only "$id" --min-runs 20 --format json --output "/tmp/$(echo "$id" | tr '/@' '__').json"
done
```

## Performance plan close-out (2026-10-21)

After the memory and libification wave (fair single-process `grit log --limit`, mapped pack index / commit-graph / MIDX, packed-refs seek, repository path cache, LTO release profile, merge tree cache, pick/revert/merge pass reduction, status/diff scan memory, add without full index copies), the factory re-ran the **cooking dashboard** matrix on **`grit 0.5.15`** (`756aa953`) vs system **`git 2.43.0`**. Committed baselines: **`suite-after-LH.json`**, **`hot-paths-after.json`**, **`commit-after-LH.json`**, and **`dashboard/current.json`**. **`grit-bench compare`** against the pre-plan dashboard snapshot at **`grit 0.5.14`** shows **no overlapping 10k scenario regressed more than 5%** on Grit median wall time.

### Synthetic L/H dashboard (before → after)

| Scenario | Git ms (before→after) | Grit ms (before→after) | Ratio before→after |
| --- | ---: | ---: | ---: |
| `status-dirty-10000` | 11.8→10.3 | 13.8→8.7 | 1.17×→**0.84×** |
| `status-clean-10000` | 8.1→8.0 | 11.4→7.6 | 1.40×→**0.95×** |
| `add-10000` | 20.1→23.1 | 33.8→30.2 | 1.68×→**1.31×** |
| `commit-10000` | 199.7→176.4 | 113.9→73.1 | 0.57×→**0.41×** |
| `switch-10000` | 23.6→21.9 | 30.2→27.1 | 1.28×→**1.24×** |
| `pick-10000` | 123.3→113.5 | 671.5→199.7 | 5.45×→**1.76×** |
| `merge-10000` | 105.1→18.5 | 732.8→28.5 | 6.97×→**1.54×** |
| `diff-10000` | 5.7→5.3 | 12.4→10.3 | 2.19×→**1.92×** |
| `log-10000` | 3.3→3.2 | 55.9→4.4 | 17.0×→**1.39×** |

### Synthetic 10k — wall time and peak RSS (before → after)

Each row is **Git → Grit** on median wall time (ms) and **peak RSS** from hyperfine + `grit-bench measure-rss` (same capture as **`dashboard/current.json`**). On this VM, Grit peak RSS exceeds **1.5×** Git on **`status-*`**, **`pick-*`**, and **`log-10000`**; **`add`**, **`commit`**, **`switch`**, **`merge`**, and **`diff`** stay below **1.5×** on RSS while several remain above **1.5×** on wall time.

| Scenario | Git ms | Grit ms | Git RSS | Grit RSS | Ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| `status-dirty-10000` | 11.8→10.3 | 13.8→8.7 | 9.8 MiB | 15.6 MiB | 0.84× |
| `status-clean-10000` | 8.1→8.0 | 11.4→7.6 | 9.2 MiB | 15.5 MiB | 0.95× |
| `add-10000` | 20.1→23.1 | 33.8→30.2 | 16.4 MiB | 19.8 MiB | 1.31× |
| `commit-10000` | 199.7→176.4 | 113.9→73.1 | 17.1 MiB | 21.0 MiB | 0.41× |
| `switch-10000` | 23.6→21.9 | 30.2→27.1 | 14.6 MiB | 19.8 MiB | 1.24× |
| `pick-10000` | 123.3→113.5 | 671.5→199.7 | 16.1 MiB | 25.3 MiB | 1.76× |
| `merge-10000` | 105.1→18.5 | 732.8→28.5 | 15.3 MiB | 19.8 MiB | 1.54× |
| `diff-10000` | 5.7→5.3 | 12.4→10.3 | 15.6 MiB | 17.1 MiB | 1.92× |
| `log-10000` | 3.3→3.2 | 55.9→4.4 | 7.0 MiB | 14.7 MiB | 1.39× |

At **100k** files the same suites land in **`dashboard/current.json`** (22 scenarios). Highlights vs the previous **`suite-after-LH.json`** / **`hot-paths-after.json`** captures: dirty/clean **status** at 100k is near parity (**~0.98–1.26×**); **`switch-100000`** and **`merge-100000`** drop to **~0.7–1.0×**; **`switch-wide-100000`** falls from **~13×** to **~1.03×**. Remaining headline gaps above **1.0×** Git time on this refresh:

| Scenario | Grit ÷ Git | Profile / why |
| --- | ---: | --- |
| `revert-100000` | **9.96×** | Merge-engine revert still replays large tree diffs; pick/revert pass sharing helped 10k more than 100k topic commits. |
| `pick-100000` | **9.20×** | Same merge/checkout path as revert on ~2000-path topics. |
| `pick-series-100000` | **5.87×** | By design **20×** separate `grit pick` processes vs one `git cherry-pick` range (process + repo open overhead). |
| `pick-series-10000` | **4.31×** | Same multi-process model at L tier. |
| `diff-100000` | **2.87×** | Parallel worktree diff still allocates per-entry results on wide modified sets (~2000 paths); RSS not yet the limiter. |
| `diff-10000` | **1.96×** | Same diff pipeline at L. |
| `add-100000` | **1.86×** | Staging ~20% of 100k paths still walks the full index; incremental index updates reduced copies but not scan volume. |
| `pick-10000` / `revert-10000` | **1.73× / 1.88×** | Residual merge + index write cost on 2000-path topics. |
| `merge-10000` | **1.51×** | Small-repo merge still above Git on median; 100k merge is parity. |
| `add-10000` | **1.49×** | Index update path at L. |
| `log-10000` / `log-100000` | **~1.37×** | Single `grit log --limit 100` vs `git log -100 --oneline`; remaining gap is revwalk + formatting, not paging. |
| `status-clean-100000` | **1.26×** | Clean 100k worktree: untracked/ignore setup vs Git preload. |

Factory follow-ups (queued as **Speed:** tasks): `diff-100000`, `add-100000`, `pick-series-100000`, `pick-100000`, `revert-100000`, `pick-series-10000`, `add-10000`, `diff-10000`, `pick-10000`, `revert-10000`, `merge-10000`, `log-100000`. The `pick-*` and `revert-*` rows were closed on 2026-10-22. See [Pick, revert and merge apply path](#pick-revert-and-merge-apply-path-2026-10-22).

### Daily common paths (roadmap 2)

**`grit-utils/baselines/daily-common-paths.json`** records six **`grit-bench daily`** matrix ids added for roadmap-2 CLI conventions: **`diff/stat@synth-100k`**, **`log/stat-n50@git-git`**, **`log/json-filter@git-git`**, **`notes/list-n100@synth-10k`**, and shallow clone over **`file://`** and grit smart HTTP (**`clone/shallow-depth1@synth-10k`**, **`clone/shallow-depth1-http@synth-10k`**). Each row includes hyperfine medians and **`peak_rss_bytes`** for grit and system git; pre-timed hooks check output equivalence where semantics match (full diff numstat paths, per-commit log stat blocks, log JSON commit oids, exact note `(commit, blob)` mappings, shallow boundary refs and `.git/shallow`). On the factory VM capture (2026-10-23), **`log/stat-n50@git-git`** is the largest time gap (**~4.3×** git); that row is queued for the speed agent rather than fixed in the registry change.

```bash
cargo build --profile dist -p grit-cli -p grit-http-server -p grit-utils
./target/dist/grit-bench daily --only 'diff/stat@synth-100k,log/stat-n50@git-git,log/json-filter@git-git,notes/list-n100@synth-10k,clone/shallow-depth1@synth-10k,clone/shallow-depth1-http@synth-10k' \
  --format json --output grit-utils/baselines/daily-common-paths.json --grit ./target/dist/grit
```

### Clone checkout of a wide tree (2026-10-24)

The daily matrix id **`clone/checkout@synth-100k`** clones a bare copy of the **100k-file** synthetic fixture by plain path, so both tools link the pack locally and the timed run is dominated by checking out every file (`git clone -q <path>` vs `grit clone <path>`). It closes issue #1678, where a 200k-file clone was **2.5×** slower than git with **~4×** git's user CPU: the populated-tree checkout writes files in chunks of 2048, and every chunk rebuilt the index cache tree from the whole commit tree (about 100 full tree walks for 200k files) only to discard it until the last chunk. Chunks now update only the in-memory index; the cache tree is built and the index is written once after the last chunk.

| Capture | git | grit | grit ÷ git | grit peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Before (`daily-clone-checkout-synth-100k-before.json`) | 2957 ms | 4269 ms | 1.44× | 150 MiB |
| After (`daily-clone-checkout-synth-100k-after.json`) | 2959 ms | 3155 ms | 1.07× | 150 MiB |

On the 200k-file repro from the issue (one commit, gc'd, 4 vCPU), `grit clone` went from **7.16 s** (user 5.34 s) to **3.12 s** (user 1.45 s), against **3.50 s** (user 1.14 s) for git; peak RSS is about **89 MB** vs **61 MB** for git. The remaining system time is concurrent file creation in the same directories across checkout workers.

```bash
./target/release/grit-bench daily --only 'clone/checkout@synth-100k' \
  --format json --output grit-utils/baselines/daily-clone-checkout-synth-100k-after.json --grit ./target/release/grit
```

### Log pages on git.git (2026-10-25)

`grit log` pages walk history in committer-date order, and the walk reads every commit it queues to learn its parents and date. The rows were then built by reading and parsing the same commits a second time, which cost about a third of a 5000-commit page. The walk now keeps the payloads it read (in one shared buffer, released as soon as the rows are loaded) and hands them to the row loader through [`LogWalkPage::commit_data`](rustdoc:grit_lib::revwalk::log_walk::LogWalkPage), so each commit is inflated once. Several fixed costs paid on every page also shrank: peeling to a commit decides the object kind from its header instead of inflating it twice, DWIM ref resolution reads each candidate (`refs/HEAD`, `refs/tags/HEAD`, …) once instead of twice, `packed-refs` records decode their ids while validating them, and pack index lookups in walk order binary-search directly instead of first comparing up to 128 rows after the previous hit.

Hyperfine medians, `dist` builds of the commit before and after the change on the pinned git.git clone (factory VM, same sitting), with median peak RSS over 15 runs:

| Command | git ms | grit before | grit after | git RSS | grit RSS before | grit RSS after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `log --limit 100 --json` (`log/json-100@git-git`) | 2.85 | 4.34 | 4.00 | 12.4 MiB | 18.5 MiB | 18.5 MiB |
| `log --limit 100` (`log/oneline-100@git-git`) | 2.87 | 3.89 | 3.66 | 12.3 MiB | 18.0 MiB | 17.9 MiB |
| `log --limit 50 --json --filter …` (`log/json-filter@git-git`) | 2.15 | 4.31 | 4.07 | 10.0 MiB | 16.8 MiB | 17.1 MiB |
| `log --limit 1000 --json` | 11.50 | 21.35 | 14.93 | 15.0 MiB | 23.4 MiB | 23.3 MiB |
| `log --limit 5000` (`log/all@git-git`) | 46.36 | 98.65 | 59.17 | 18.1 MiB | 31.1 MiB | 31.0 MiB |

Path-limited, `--author`, `--stat`, `revs` and `show` rows are unchanged within noise (`daily-log-git-git-before.json` / `daily-log-git-git-after.json` hold the full daily matrix capture for both binaries). What remains of the 100-commit gap is mostly fixed cost: argument parsing, repository discovery and config loading, and listing `packed-refs` for the decorations in the JSON rows.

```bash
cargo build --profile dist -p grit-cli -p grit-utils
./target/dist/grit-bench daily --only 'log/json-100@git-git,log/oneline-100@git-git,log/all@git-git,log/author@git-git,log/json-filter@git-git,log/limit-path@git-git,log/path@git-git,log/stat-n50@git-git' \
  --min-runs 20 --format json --output grit-utils/baselines/daily-log-git-git-after.json --grit ./target/dist/grit
```

### git.git headlines (hyperfine wall time + peak RSS)

**`grit-utils/baselines/dashboard/git-dot-git-headlines.json`** uses the standard grit-bench schema (hyperfine medians + **`peak_rss_bytes`** per side). Headline wall times are **not** taken from the daily RSS poll loop (which quantized sub-50 ms commands to ~50 ms); they use the same hyperfine + **`measure-rss`** path as other scenario suites.

| Scenario | Git ms | Grit ms | Git RSS | Grit RSS | Ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| `log/oneline-100@git-git` | 3.44 | 5.36 | 14.0 MiB | 24.4 MiB | 1.56× |
| `show/head@git-git` | 2.02 | 3.77 | 7.4 MiB | 14.8 MiB | 1.87× |
| `resolve/head-n@git-git` | 1.25 | 3.21 | 7.4 MiB | 12.5 MiB | 2.57× |
| `branch/list@many-refs` | 1.41 | 8.61 | 7.2 MiB | 15.8 MiB | 6.10× |

**`dashboard/git-dot-git-worktree-status.json`** captures clean/dirty **status** (and related worktree rows) on the pinned **git.git** clone; every git/grit side includes **`peak_rss_bytes`** (e.g. clean status **~27 MiB** on both tools on this VM).

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench fixtures prepare --only git-git,git-git-wt,many-refs
./target/release/grit-bench status --sizes 10000,100000 --format json --output /tmp/status.json
./target/release/grit-bench hot-paths --sizes 10000,100000 --format json --output grit-utils/baselines/hot-paths-after.json
./target/release/grit-bench headlines --offline --format json --output grit-utils/baselines/dashboard/git-dot-git-headlines.json
```

## grit-bench vs Git

### Methodology

- **Build profile:** Command-level numbers use a **`dist`** **`grit`** (**`cargo build --profile dist -p grit-cli`**, or **`make build`**): **`lto = "fat"`**, **`codegen-units = 1`**, **`opt-level = 3`**, **`strip = true`**, **`panic = "unwind"`** (required because **`grit-lib`**'s parallel pool uses **`catch_unwind`** / **`resume_unwind`**). Workspace **`[profile.release]`** stays fast for **`cargo test --release`** (no fat LTO on test binaries). Rebuild **`grit-cli`** before re-running **`grit-bench`**; pass **`--grit target/dist/grit`** or rely on the harness preferring **`target/dist/grit`** over **`target/release/grit`**.
- **Trivial status RSS:** In an empty repo, peak RSS for **`grit status`** is measured with **`grit-bench measure-rss`** (fresh helper process, **`GRIT_BENCH_MEASURE_CMD='…/target/dist/grit status'`**) and/or **`VmHWM`** from **`/proc/<pid>/status`** over many cold starts (100+). Report median and max in bytes or KiB. **Target remains ≤ 8 MiB (8 388 608 bytes)**; the **`dist`** profile reaches **~9.4–9.8 MiB** on the factory VM (link tuning only). **Follow-up:** factory plan step under **[#2348](https://github.com/gitbutlerapp/grit/issues/2348)** — binary diet (feature-gate or lazy-load HTTP/jaq, reduce static init/relocations, allocator/startup mapping review) without slowing dashboard scenarios.
- **Fixtures** use synthetic repos at large file counts and deep history, built deterministically in a scratch directory.
- **Drivers** are CLI invocations of `grit` and `git` with equivalent semantics (not argv-for-argv compatibility). Dashboard scenarios `log-{size}` time a single **`grit log --limit 100`** against **`git log -100 --oneline`** on the synthetic history fixture. The production **campaign** matrix still has a separate **`log`** row that uses paged **`grit log --before=`** cursors on **git.git** (not the dashboard id).
- **Ratios** are **Grit median ÷ Git median** (stored in each scenario’s `ratio` field; hyperfine also records mean and spread). Values below `1.00×` mean Grit is faster; values above `2.00×` are highlighted as regressions worth investigating.
- **Spread** shows Grit’s standard deviation across timed runs (hyperfine `--min-runs` / warmup settings from the baseline capture).

**Revision-selection filters (`grit-bench revision-selection`).** Baseline **`grit-utils/baselines/revision-selection.json`** captures the deep-history network fixture with and without a reachable **commit-graph**: bounded **`--since`** counts, symmetric-range **left/right** counts, **`--author`** counting and **`-n 50`** listing (library **`lib-op`** drivers for rev-list-shaped workloads; **`grit log --author`** / **`grit revs --author`** for CLI rows), plus **`log-author-n50`** on the same fixture. Daily census registers the same workloads under **`REVISION_SELECTION_MATRIX_SCENARIOS`** (ids such as **`revs/count-since@deep-history`**, **`log/author-n50@deep-history-cg`**, and **`log/author@git-git`**). Pickaxe, **`--group-by author`**, **`--follow`**, and **`--cherry-pick`** scenarios join this baseline when their roadmap steps land.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench revision-selection --format json --output grit-utils/baselines/revision-selection.json
```

### Maintenance (`grit-bench maintenance`)

Baseline **`grit-utils/baselines/maintenance-medium.json`** compares full **`grit gc`**, **`grit gc repack --bitmaps`**, and **`grit gc commit-graph`** against **`git gc`**, **`git repack -adb`**, and **`git commit-graph write --reachable --changed-paths`** on the deep-history network fixture with a worktree seeded with extra loose objects. Each hyperfine run uses **`prepare-maintenance`** to restore that worktree so timings are not skewed by mutating the same checkout across iterations.

Recorded on the factory VM when the baseline was captured (see `machine` / `tools` in the JSON). Roadmap item **15** targets **≤ 1.1×** Git on large-repo scenarios; maintenance work is tracked separately until repack and commit-graph close the gap:

| Scenario | Grit ÷ Git (committed baseline) | vs 1.1× bar | Follow-up |
| --- | ---: | --- | --- |
| `gc-medium` | **0.30×** | meets | — |
| `repack-medium` | **1.66×** | miss | [GitHub #1026](https://github.com/gitbutlerapp/grit/issues/1026) — pack build / bitmap sidecars |
| `commit-graph-write` | **1.17×** | miss | [#1026](https://github.com/gitbutlerapp/grit/issues/1026) — changed-path Bloom generation |

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench maintenance --format json --output /tmp/maintenance-medium.json
```

### Reproduce locally

Build release binaries, install [hyperfine](https://github.com/sharkdp/hyperfine), then run matching grit-bench scenarios from the repo root (see `grit-utils/README.md` for the current subcommands):

```bash
cargo build --profile dist -p grit-cli -p grit-utils

# Example: hot-path scenarios at L/H tiers (switch, pick, merge)
./target/release/grit-bench hot-paths --sizes 10000,100000 --format json --output /tmp/grit-bench-hot-paths.json
```

**LTO / dist profile (factory step 2370, factory VM):** Before profile tuning, a default **`release`** **`grit`** had **19 208 934** bytes of `.text` (GNU **`size`**, ~18.3 MiB), **28 642** dynamic relocations, **24 924 840** bytes on disk, **~11.7 MiB** median peak RSS on trivial **`grit status`** (**`measure-rss`**, empty repo), and **~184 s** clean **`cargo build --release -p grit-cli`**. The shipped **`dist`** profile cuts **`.text`** to **17 360 233** bytes (~16.5 MiB), relocations to **17 189**, on-disk size to **17 691 672** bytes, trivial status RSS to **~9.8 MiB** median (**`measure-rss`** **10 233 856** bytes; **`VmHWM`** **9996 KiB**), and clean **`cargo build --profile dist -p grit-cli`** to **~1.4×** the pre-LTO **`release`** build. **`opt-level = "s"`** was slower than **`opt-level = 3`** on every dashboard **10k** scenario on this VM, so **`dist`** uses **`3`**. RSS is still above the **8 MiB** goal; see the follow-up under **[#2348](https://github.com/gitbutlerapp/grit/issues/2348)**.

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

### pick-range-20 (RSS gap)

**Scenario:** `grit pick base..topic` vs `git cherry-pick --no-edit base..topic` on the **pick-series** synthetic-**10000** fixture (20 commits on `topic`, one file touched per commit). Baseline: **`grit-utils/baselines/hot-paths-after.json`** (`pick-range-20`).

**Committed baseline (factory VM, release hyperfine, Git 2.43, 2026-10-24):** grit median **~94 ms** vs git **~118 ms** (**~0.79×** time); peak RSS **~1.42×** git (grit **~22.4 MiB** vs git **~15.8 MiB**). The earlier ~1.24× figure came from a `dist` (LTO, stripped) build with a smaller resident binary. Median time meets the **≤ 1.0×** goal; peak RSS remains above **≤ 1.2×** until shared replay/checkout work lands.

**Follow-up:** [GitHub issue #2977](https://github.com/gitbutlerapp/grit/issues/2977) — cross-cutting replay/merge work (index-hinted tree diffs in replay merge, scoped checkout for changed paths only, reduce peak RSS by avoiding full tree/index copies per step). Benefits **revert**, **merge**, and **rebase**, not pick-only code.

**Regression guard:** After merge-tree cache reuse and touched-path collection changes, **`pick-10000`** and **`merge-10000`** ratios on the same fixture did **not** regress vs the prior baseline (fresh hot-path run, 2026-10-23).

### Follow-up tracking (step 386)

**Binary diet for trivial status RSS (factory [#2348](https://github.com/gitbutlerapp/grit/issues/2348), after step 2370):** Reach **`grit status`** peak RSS **≤ 8 MiB** (8 388 608 bytes) in an empty repo without regressing any dashboard **10k** scenario. Candidates: feature-gate or lazy-load the HTTP (**`http-ureq`** / TLS) and **`jaq`** stacks, trim static initialization and dynamic relocations further, and review allocator/startup mappings. **Do not** lower the **8 MiB** target; step 2370 accepted **~9.4–9.8 MiB** as the link/profile outcome only.


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

The **`grit-bench refs`** suite compares library drivers (`grit-bench refs-driver …`) to **`git for-each-ref`** and **`git update-ref --stdin`** on bare repositories with **100 000** branch refs. Fixtures are built once with system git and cached under the bench scratch directory: fully **packed** refs, **loose** refs only, and (when system git is **≥ 2.45**) a **reftable** layout. Scenarios list all refs, list the **`refs/heads/b05*`** glob (~1 000 refs), batch-**create** 100 000 refs into an empty-refs working copy, **update** 1 000 existing refs, and **delete** 1 000 refs; a prepare hook restores the fixture before each timed iteration. Initial packed/loose listing baselines are in **`grit-utils/baselines/refs-before.json`**; files-backend listing and batch **`update-ref`** optimizations on the 100k-ref fixtures are in **`grit-utils/baselines/refs-after.json`**; reftable timings (Git **≥ 2.45**) are in **`grit-utils/baselines/refs-after-reftable.json`**. The **`grit-utils/baselines/refs-after-994.json`** snapshot captures [`gc::update_refs`](rustdoc:grit_lib::refs::gc::update_refs) batching through [`commit_ref_store_batch`](rustdoc:grit_lib::refs::commit_ref_store_batch) with a reused packed-refs cache instead of reparsing `packed-refs` per update.

On the **100k packed** fixture, **update 1 000 refs** (`update-ref-update-1k-refs-100k-packed`) dropped from **~184 s** Grit mean (4 485× git) to **~1.5 s** (**~36×** git at ~41 ms); the regression was repeated full `packed-refs` parses per update in the batch path, not transaction semantics.

The same fixtures serve two **`grit upload-pack`** scenarios against **`git upload-pack`**. The first is a protocol v2 **`ls-refs`** request with `peel`, `symrefs` and **`ref-prefix refs/heads/b05000`**, the request `git fetch origin b05000` sends (`upload-pack-ls-refs-prefix-*`). The second is the full v0 advertisement (`upload-pack-advertise-v0-*`). Both are read-only, so they run without the restore hook. Set **`GRIT_BENCH_REFS_ONLY_SERVE=1`** to run only these. Before [#1820](https://github.com/gitbutlerapp/grit/issues/1820), `ls-refs` listed and peeled every ref and only then applied the prefix: on the packed fixture it took **387 ms** for one ref (git **1.0 ms**), and **1.15 s** on the loose fixture (git **242 ms**). It now asks the ref store for the requested prefix only and peels only when the client asks. That brings it to **4.0 ms / 19.6 MB** packed and **42.5 ms / 12.9 MB** loose, where it beats git (**277 ms**) because the loose walk skips sibling ref files by name. The packed v0 advertisement went from **414 ms** to **117 ms** (git **16 ms**) because `packed-refs` peel lines replace tag reads. Before and after are in **`grit-utils/baselines/refs-serve-before.json`** and **`grit-utils/baselines/refs-serve-after.json`** (system `git 2.43.0`, one sitting).

```bash
cargo build --release -p grit-cli -p grit-utils
GRIT_BENCH_REFS_ONLY_SERVE=1 GRIT_BENCH_BRANCH=factory/ls-refs-prefix-first ./target/release/grit-bench --min-runs 10 refs --format json --output grit-utils/baselines/refs-serve-after.json
```

Criterion micro-benchmarks for the reftable backend: **`cargo bench -p grit-lib --bench refs_reftable`** (`for_each_ref_100k`, `prefix_seek`, `transaction_create_10k`).

```bash
cargo build --release -p grit-cli -p grit-utils
GRIT_BENCH_BRANCH=factory/refs-bench-774 ./target/release/grit-bench refs --format json --output grit-utils/baselines/refs-before.json
GRIT_BENCH_BRANCH=factory/files-refs-perf-782 ./target/release/grit-bench refs --format json --output grit-utils/baselines/refs-after.json
GRIT_BENCH_BRANCH=factory/reftable-refs-perf-783 GRIT_BENCH_REFS_ONLY=reftable ./target/release/grit-bench refs --git "$(command -v git)" --format json --output grit-utils/baselines/refs-after-reftable.json
```

Use a **≥ 2.45** system **`git`** (or an explicit `--git` path) when refreshing the reftable baseline; older git skips the reftable fixture.

Set `GRIT_BENCH_BRANCH` to the GitButler branch you are benchmarking so `grit_commit` in the JSON matches that branch tip (required on `gitbutler/workspace` checkouts). When `--grit` points at a binary built from a different commit than `grit-bench` itself (for example a before/after pair timed with one harness), set `GRIT_BENCH_REPORT_GRIT_COMMIT` to that binary's commit; every suite, including `grit-bench daily`, records it as `grit_commit`.

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

## grit-bench: tree-sha256 tags

Scenario group **`tag-tree-sha256`** (`grit-bench tag-tree-sha256`) measures signed annotated tags with optional [tree-sha256](../library/tree-sha256.md) claims on **git-dot-git**, **chromium-synth**, and optionally **linux** worktrees (same fixture prep as the [real-world campaign](#real-world-campaign)).

| Scenario id | What it measures |
| --- | --- |
| `<fixture>/tag-verify-digest-no-signature` | `grit tag --verify --no-signature` vs a git-only `ls-tree` / `cat-file` / SHA-256 reproduction pipeline |
| `<fixture>/tag-verify-digest-reference` | Same grit verify path vs the reference `tree-sha256` binary when installed (skipped otherwise) |
| `<fixture>/tag-sign-tree-sha256` | `grit tag --sign --tree-sha256` total time vs `grit tag --sign` only (overhead should stay within recomputed hash time + **10 ms**) |
| `<fixture>/tag-verify-signed` | Full `grit tag --verify` vs `git verify-tag` on a signed tag carrying a tree-sha256 header |

**Acceptance bars (factory VM):** digest verify ≤ **1.2×** reference wall time when the reference tool is present, and ≤ the git-only pipeline on all three shapes; signed verify ≤ **max(signature, hash) + 10 ms** overhead on the sign scenario; peak RSS for each row is recorded in the committed JSON (`peak_rss_bytes`).

```bash
cargo build --release -p grit-cli -p grit-utils
export GRIT_BENCH_ODB_CACHE=/tmp/grit-bench-odb-cache
./target/release/grit-bench tag-tree-sha256 --format json --output grit-utils/baselines/tag-tree-sha256.json
# optional linux row:
GRIT_BENCH_LINUX=1 ./target/release/grit-bench tag-tree-sha256 --with-linux --format json --output grit-utils/baselines/tag-tree-sha256.json
```

Committed baseline: **`grit-utils/baselines/tag-tree-sha256.json`** (listed in `content/docs/site.toml`).

Criterion micro-benchmark: `cargo bench -p grit-lib --bench tree_sha256` (compare with `--baseline tree_sha256-before` when changing hot paths).

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
