---
title: Benchmarks
summary: Grit vs Git on realistic workloads, from the committed grit-bench baseline.
---

Performance work is measured on the factory VM and in CI against the **system** `git` binary. Micro-benchmarks live in `grit-lib` (Criterion) and command-level scenarios in `grit-bench` (`grit-utils`).

For a map of **grit-lib** modules, **grit** commands, common workflows, and every **`grit-bench daily`** scenario id, see [`grit-utils/MATRIX.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/MATRIX.md). The same ids appear with **time and memory ratios** on the [Performance overview (daily matrix)](performance-overview.md) page.

Command-level numbers below come from the committed **grit-bench** JSON baselines listed in `content/docs/site.toml`. Scenario suites use **hyperfine** against the system **`git`** binary and a release **`grit`** build on the same machine; individual studies below document their sampling method. Times are wall-clock milliseconds.

## Symmetric revision counts (2026-10-27)

Resolving `HEAD~500` parsed its parent chain, then the symmetric-range walk read
those commits again for the same parents and timestamps. Symmetric range
expansion now seeds the repository's existing metadata cache, capped at 1,024
entries. Reuse is guarded when shallow boundaries, promisor state, grafts or
replacements can change parent semantics; standalone resolution keeps its original
retention policy.

Measured against system Git 2.55.0 on a four-vCPU AMD EPYC 9V45 Linux VM with
16 GiB RAM and Rust 1.99.0 `release` builds. Frozen main is `aa1964221e04`.
The production deep-history fixture has 50,000 commits and 20,000 files; git/git
is pinned to v2.47.0 (`777489f9e09c`). All builds and tests
were stopped during measurements. Both grit executables were copied using the
same buffered-write method and warmed equally.

The table uses **30 rotated, interleaved runs per tool** after three warmups.
Wall time is the median from Python `perf_counter_ns` around `/usr/bin/time -v`,
including the helper's launch. Memory is the median process peak RSS in KiB.
The target improves **16.1%**; peak RSS is essentially unchanged.
Heaptrack records **18,591 → 13,586 allocation calls (26.9% fewer)**, with peak
heap usage remaining about 0.64 MB. The scoped cache avoids extra retention in
the standalone resolution controls.

| Scenario | Git ms | Main ms | Fixed ms | Main peak KiB | Fixed peak KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `revs/left-right-count@deep-history` | 3.054 | 6.235 | 5.229 | 15,208 | 15,172 |
| `revs/author-n50@deep-history` | 2.414 | 4.559 | 4.479 | 16,884 | 16,960 |
| `revs/count-author@deep-history` | 99.796 | 127.828 | 127.825 | 25,566 | 25,540 |
| `revs/author-n50@deep-history-cg` | 2.551 | 4.359 | 4.305 | 19,524 | 19,518 |
| `revs/count-author@deep-history-cg` | 106.614 | 129.287 | 128.597 | 25,722 | 25,710 |
| `revs/count-since@deep-history` | 2.234 | 3.576 | 3.488 | 14,724 | 14,612 |
| `revs/count@git-git` | 424.569 | 575.766 | 579.342 | 125,950 | 125,962 |
| `revs/count-path-head@git-git` | 320.630 | 343.816 | 344.814 | 270,582 | 270,540 |
| `resolve/ancestor-500@deep-history` | 2.767 | 4.660 | 4.676 | 14,824 | 14,838 |
| `resolve/ancestor-10000@deep-history` | 16.161 | 28.993 | 29.148 | 16,296 | 16,216 |
| `resolve/head@deep-history` | 2.000 | 3.559 | 3.587 | 12,676 | 12,564 |
| `revs/left-right-first-parent@deep-history` | 3.060 | 6.297 | 5.242 | 15,208 | 15,096 |
| `log/oneline-100@git-git` | 3.830 | 5.175 | 5.078 | 21,120 | 21,008 |
| `show/head@git-git` | 2.605 | 3.899 | 3.837 | 16,324 | 16,276 |

A targeted direct-spawn daily recheck measures the target at
**5.822 → 4.361 ms**
(20 runs per build; system Git **2.643 → 2.345 ms**).
The earlier full-matrix captures experienced host drift, so the interleaved table
is the primary before/after comparison.
All measured grit outputs match frozen main byte for byte, and every count
matches system Git. Library tests cover reuse, standalone retention, invalid
objects, shallow and replacement guards, plus Git/grit object round trips and
navigation through loose objects, packs, commit graphs, tags and merged histories.

The registered `revs/count-path@git-git` row currently references `7e34d81`, absent
from the pinned fixture, so system Git rejects it. The table labels the working
`HEAD` path-count control separately.

Raw samples, commands, binary/source hashes and profiling counts:
[`revs-navigation-4759-paired.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/revs-navigation-4759-paired.json).
The standard runner captures are
[`daily-revs-navigation-4759-before.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/daily-revs-navigation-4759-before.json)
and
[`daily-revs-navigation-4759-after.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/daily-revs-navigation-4759-after.json).

## Inexact rename detection (2026-10-27)

Issue [#1683](https://github.com/gitbutlerapp/grit/issues/1683) reproduced on
`git/git`: line hashes were cached per blob, but every inexact candidate pair
still allocated and populated a new hash map. Rename and copy detection now
sort and combine byte weights once per blob and intersect the immutable tables
without allocating per pair. Index advancement avoids unpredictable hash-order
branches. Line hashing, CRLF normalization, scores, thresholds and tie-breaking
are unchanged.

Measured on a four-vCPU AMD EPYC 7763 64-Core Processor Linux VM with 16 GiB RAM, Rust 1.99.0 `release`
builds and stock Git 2.55.0. Frozen `origin/main` is `80dc322f7c2f`.
The upstream fixture is `git/git` at `37b3475fb530` (82,605 commits), comparing
`v2.40.0` with `master`. The second fixture moves 1,000 documents containing
100 unique lines each and edits one line per document, with
`diff.renameLimit=2000`.

After one warmup per tool, five interleaved runs alternate system Git, frozen
main and the fix, reversing order on alternate rounds. Builds and tests are
paused. Wall time is the median measured with Python `perf_counter` around
`/usr/bin/time -v`; RSS is the maximum process peak across those runs, in KiB.
The name-status cases use `-M`; the other tag cases use the listed output mode
with default rename detection unless marked `no-renames`.

| Scenario | Git ms | Main ms | Fixed ms | Git RSS KiB | Main RSS KiB | Fixed RSS KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| name-status | 313.5 | 1248.9 | 180.1 | 56,616 | 79,828 | 79,620 |
| name-only | 312.7 | 1594.8 | 208.4 | 56,560 | 80,320 | 80,508 |
| numstat | 898.9 | 2308.4 | 944.0 | 118,724 | 171,336 | 170,220 |
| stat | 900.2 | 2312.8 | 922.6 | 118,896 | 171,064 | 169,916 |
| patch | 1179.0 | 2730.9 | 1352.4 | 118,528 | 203,728 | 203,992 |
| name-only-no-renames | 8.8 | 13.8 | 14.2 | 26,852 | 38,164 | 38,292 |
| numstat-no-renames | 662.6 | 728.9 | 721.5 | 119,188 | 158,940 | 159,096 |
| inexact-1000 | 319.8 | 5549.2 | 642.8 | 13,996 | 47,024 | 46,836 |

The tag name-status comparison is **6.9× faster than main** and **0.57× Git's
time**. The 1,000-file case improves **8.6×**, to **2.01× Git's time**. Rename-only
peak RSS remains essentially unchanged. The candidate matrix still uses more
memory than Git on the 1,000-file fixture; this change removes per-comparison
allocations without changing candidate storage. No-renames times differ by at
most 0.4 ms for name-only and 1.1% for numstat in this run.

Every output mode is byte-for-byte identical between main and the fix. Both
grit builds report 937 renames for the tag pair versus Git's 936, preserving the
existing similarity heuristic. All 1,000 synthetic rename path pairs match Git.
Rust tests check weighted line multiplicities, unterminated lines, binary bytes,
symmetry, table reuse and CRLF handling. An allocation regression covers both
rename and copy matrices against Git-written objects and checks rename mappings
with system Git.

Raw samples, complete GNU time output, commands, fixture generation and output
hashes: [`rename-scoring-1683.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/rename-scoring-1683.json).
Criterion mean estimates improve from **90.79 to 9.42 ms** for 64 documents
with shared paragraphs and **394.72 to 46.67 ms** for 256 documents with unique
paragraphs. Both binaries run with builds and tests paused (3-second warmup,
10 samples). The existing 2,000 × 2,000 matrix remains in the suite:

```bash
cargo bench -p grit-lib --features http-ureq,tracing --bench diff_renames -- diff_renames_adoc_similar
```

## Scattered pack-index lookups and log memory (2026-10-27)

Large pack indexes now predict an object's row from its hash prefix, then bracket
it with exponentially growing steps and finish with binary search. This avoids
reading both ends of a fanout bucket and dividing wide integers on each probe.
Every result still compares the complete object id; clustered hashes, missing ids,
SHA-1/SHA-256 and v1/v2 indexes retain their existing behavior. Bracketing and
binary search bound the probe count logarithmically.

On a 4-vCPU AMD EPYC 7763 Linux VM with 16 GiB RAM, Rust 1.99.0 `release` builds
and system Git 2.55.0, the pinned git/git fixture's 100-commit log uses **1,798 KiB
less peak RSS (8.2%)**. Headline elapsed time is essentially unchanged: a separate
300-run Hyperfine check measured 5.132 → 5.126 ms (Git: 3.606 ms). Startup and object decompression remain larger costs.

The table uses 20 rotated, interleaved runs per binary after three warmups, with
builds and tests stopped. Frozen main is `7cc7979535ce`; fixed output matches it
byte for byte in every row. Python `perf_counter` wraps `/usr/bin/time -v`, so these
wall times include that helper's launch; RSS is median peak RSS in KiB. Executables
were copied with the same buffered-write method and warmed equally. Mapped code
and file pages introduce RSS variation; the small cat/status differences are below
0.3 MiB. The path-limited row's roughly 1% timing difference did not persist in a
separate 100-run check (22.3 → 21.9 ms).

| Scenario | Git ms | Main ms | Fixed ms | Main KiB | Fixed KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `log/all@git-git` | 53.14 | 73.62 | 73.56 | 35,422 | 33,894 |
| `log/author-n50@deep-history` | 3.13 | 5.49 | 5.43 | 17,124 | 15,888 |
| `log/author-n50@deep-history-cg` | 3.51 | 5.65 | 5.60 | 19,230 | 18,122 |
| `log/author@git-git` | 3.98 | 6.65 | 6.50 | 21,232 | 19,000 |
| `log/json-100@git-git` | 4.73 | 7.01 | 6.88 | 22,752 | 20,582 |
| `log/json-filter@git-git` | 3.89 | 7.20 | 7.19 | 21,620 | 19,630 |
| `log/limit-path@git-git` | 25.51 | 22.02 | 22.23 | 36,284 | 35,424 |
| `log/oneline-100@git-git` | 4.76 | 6.26 | 6.15 | 21,986 | 20,188 |
| `log/oneline-100@synth-100k` | 3.99 | 5.17 | 5.14 | 16,512 | 15,604 |
| `log/path@git-git` | 366.65 | 450.93 | 446.27 | 244,442 | 243,682 |
| `log/stat-n50@git-git` | 102.16 | 392.18 | 389.08 | 113,432 | 110,746 |
| `show/head@git-git` | 3.37 | 4.67 | 4.66 | 16,256 | 15,504 |
| `blame/file@git-git` | 313.05 | 308.04 | 303.46 | 234,546 | 233,630 |
| `revs/count@git-git` | 540.46 | 767.01 | 756.98 | 126,158 | 126,128 |
| `cat/blob@git-git` | 2.73 | 4.03 | 4.05 | 13,308 | 13,482 |
| `status/clean@synth-100k` | 124.51 | 105.37 | 104.76 | 34,134 | 34,388 |

Raw samples and argv are in
[`log-index-4729-paired.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/log-index-4729-paired.json).
The standard daily captures are `daily-log-index-4729-before.json` and
`daily-log-index-4729-after.json`; the latter also includes the runner's paired
headline baseline. Its extra baseline has three samples, so use the 20-run table
and 300-run Hyperfine check above for the before/after comparison.

A warm loop over the same 100 real commit ids, repeated 10,000 times, measured
about **78 → 24 ns per lookup**. The standalone Criterion target covers a broader
4,096-id scattered working set, plus hot hit/miss controls, without preparing the
objects suite's unrelated pack fixtures. On the real index, the fixed scattered
hit/miss batches measured 260/289 µs per 4,096 lookups (about 64/71 ns per lookup).

```bash
# Default: deterministic 200k-object index.
cargo bench -p grit-lib --bench pack_index
# Real repository: choose one of its .idx files.
GRIT_PACK_INDEX_BENCH=/path/to/repo/.git/objects/pack/pack-HASH.idx \
  cargo bench -p grit-lib --features http-ureq,tracing --bench pack_index
```

## Multi-pack-index preparation and clean status (2026-10-26)

Issue [#2064](https://github.com/gitbutlerapp/grit/issues/2064) exposed three MIDX
mappings and repeated checksum scans during a clean status check, plus a whole-file
allocation just to read pack names. MIDX preparation and reads now use the owning
repository's `PackStore`. Strict and permissive views share validated layer backing
while retaining their own chain acceptance and diagnostic behavior. Checksums remain
mandatory, with a 2 MiB residency budget and checksum-only pages released afterward.
Pack-name reads reuse prepared metadata or fault in only the relevant mapped pages.

Measured on a 4-vCPU AMD EPYC 7763 Linux VM with 16 GiB RAM, Rust 1.99.0 `release`
builds and stock Git 2.55.0. The exact issue fixture is git/git at `7e34d817`, with
82,563 reachable commits, 422,320 packed objects, packed refs, a changed-path commit
graph, and a Git-written 12 MB MIDX. Frozen `origin/main` (`dbdf37d29`) and fixed
binaries were run in one sitting with builds and tests paused. Each cell is the
median of seven rotated warm-cache runs after three warmups. Python `perf_counter`
wraps `/usr/bin/time -v`, including process launch; RSS is in KiB.

| Status / MIDX | Git ms | Main ms | Fixed ms | Git RSS | Main RSS | Fixed RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| human / present | 13.99 | 45.28 | 24.56 | 7,664 | 61,912 | 17,588 |
| short / present | 13.48 | 44.00 | 24.35 | 7,580 | 61,912 | 17,848 |
| JSON / present | 13.59 | 44.02 | 24.45 | 7,704 | 61,884 | 17,716 |
| Markdown / present | 13.82 | 44.07 | 24.36 | 7,696 | 61,780 | 17,892 |
| human / absent | 13.21 | 15.78 | 15.89 | 7,580 | 16,340 | 17,084 |

Git's JSON and Markdown controls use ordinary `git status`; the short control uses
`git status --short`. All present/absent samples for every format are committed in
`grit-utils/baselines/midx-status-2064.json`. Human status removes **98.9% of the
MIDX-only RSS increase** (45,572 → 504 KiB), reduces total RSS by **71.6%**, and
reduces elapsed time by **45.8%**. Without MIDX, elapsed time remains near 15.8 ms;
the measured release-binary RSS increases by 744 KiB. The remaining base footprint
and fresh-process checksum cost leave room for further work against Git.

Criterion separates fresh repository-cache preparation from warm lookups (20
samples, 1-second warmup, 2-second measurement, OS caches warm). Preparing strict
and permissive views over the real 12 MB MIDX improves **14.69 → 7.36 ms**. Warm
lookups on the existing ten-pack fixture improve **96.99 → 59.27 ns** for hits and
**82.42 → 47.26 ns** for misses. The unchanged all-pack-index hit control measures
29.63 → 29.62 ns; its tiny miss control measures 5.32 → 6.23 ns.

```bash
GRIT_MIDX_BENCH_OBJECTS=/path/to/git/.git/objects \
  cargo bench -p grit-lib --bench midx -- midx_prepare
cargo bench -p grit-lib --bench midx -- midx_lookup
cargo test -p grit-cli --test status_midx_memory -- --nocapture
```

The Linux RSS regression uses 420,000 historical blobs and a one-file clean
checkout, exercising human, short, JSON, and Markdown output with MIDX present and
absent. Library regressions cover one mapping per owner, explicit and implicit
caller contexts, concurrent views, independent stores, SHA-1/SHA-256, refresh,
missing/malformed layers, diagnostics, corruption fallback, and system-Git fsck
round trips.

## Ignore path normalization (2026-10-26)

Ignore queries used to canonicalize the same worktree root for every path. The library now checks the normalized path against the root already resolved when opening the repository, retaining the filesystem-alias fallback. On the daily 1,000-path fixture this removes **4,000 `readlink` calls** and **10,000 allocation calls**; heaptrack peak heap stays at **795,340 bytes**.

Baseline: `origin/main` at `c578daa58ced74ce4a33327942e29e8409b3cb10`; change: `786ec07eeb6b34f840112964d0dcf7f5d6ea4391`. Frozen before/after binaries and stock Git 2.55.0 were measured in one sitting on an Intel Xeon Platinum 8573C VM. Builds and tests were paused during timed measurements. Both the development release profile and the shipping `dist` profile were measured.

The daily harness uses its existing shell/xargs commands and 20 alternating samples per tool. Shipping-profile medians improve **8.84 → 7.11 ms** (Git: **2.50–2.53 ms**); helper-inclusive peak RSS is **14,624 → 14,560 KiB**. Release medians improve **9.27 → 7.29 ms** (Git: **2.50 ms**). The committed daily reports retain every sample.

The table below uses 50 direct process invocations per tool, rotating tool order after five warmups. These omit the daily shell/xargs overhead. RSS is the median of ten per-process peak readings from `/usr/bin/time -v`; all ten readings and wall-time samples are in `ignore-normalization-dist.json` and `ignore-normalization-release.json`. Git uses `check-ignore -q --stdin`; grit uses `ignore check --all`, with output discarded.

| Profile / workload | Git ms | Grit before ms | Grit after ms | Git peak KiB | Grit before peak KiB | Grit after peak KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| dist / `check-1000@synth-10k` | 2.21 | 7.56 | 5.72 | 4,644 | 10,312 | 10,218 |
| dist / `siblings-1000` | 1.25 | 5.39 | 3.99 | 4,506 | 9,974 | 10,004 |
| dist / `deep-1000` | 1.63 | 11.35 | 9.50 | 4,426 | 10,398 | 10,482 |
| dist / `tracked-1000` | 10.66 | 4.92 | 3.50 | 4,608 | 9,920 | 10,038 |
| dist / `check-1@synth-10k` | 1.02 | 1.55 | 1.53 | 4,618 | 9,500 | 9,484 |
| release / `check-1000@synth-10k` | 2.20 | 7.95 | 6.07 | 4,618 | 10,126 | 12,724 |
| release / `siblings-1000` | 1.25 | 5.80 | 4.29 | 4,460 | 9,776 | 12,532 |
| release / `deep-1000` | 1.61 | 11.82 | 9.96 | 4,444 | 10,382 | 13,038 |
| release / `tracked-1000` | 10.43 | 5.12 | 3.68 | 4,594 | 9,730 | 12,276 |
| release / `check-1@synth-10k` | 1.01 | 1.77 | 1.70 | 4,610 | 9,166 | 11,924 |

All five workloads improve in wall time. Shipping-profile RSS stays within the spread of repeated process measurements; the single-path case is near the timing noise floor. The non-LTO release build increases RSS by about **2.6 MiB** despite unchanged peak heap and lower allocation traffic. This profile-dependent binary footprint remains a limitation; this change does not claim to close the memory gap to Git.

The daily fixture queries 1,000 distinct parents, mostly absent, in the standard 10k-file repository with an emptied index and `*.txt`. Auxiliary fixtures use 1,000 existing files across ten directories, 1,000 files below twelve nested directories, the same wide fixture staged with `git add -f .`, and a single path from the daily fixture. All auxiliary ignore files contain `*.txt`.

Reproduce the target with each frozen binary:

```bash
cargo build --release -p grit-cli -p grit-utils
cargo build --profile dist -p grit-cli -p grit-utils
grit-bench daily --only ignore/check-1000@synth-10k --grit /path/to/frozen/grit --output ignore.json
```

## Clean wide content merges

Issue [#2036](https://github.com/gitbutlerapp/grit/issues/2036) measures 20,000 small text files
where one side prepends a line and the other appends one. Rename detection and result-tree
composition now borrow existing paths and entries, parsed trees are shared with the ODB,
and checkout write jobs borrow their diff records. Side maps are released before composing
the result tree.

Factory VM, 2026-10-26: 4 vCPU AMD EPYC 7763, 16 GiB RAM, Linux x86_64,
Rust 1.99 `release` profile, system Git 2.55.0. One warmup precedes four interleaved
fresh-process runs of Git, origin/main (`74adad7a`), and the fix in the same worktree,
reset before each merge. Compilation is paused. Wall time uses Python `perf_counter`;
peak RSS uses `/usr/bin/time -v`. The packed row reproduces the existing daily
`merge/content-both-sides@wide-20k` fixture (200 directories, 100 files each, `git gc`
before merging). The loose rows follow the issue's reproduction at 10k and 20k files.

| Files / layout | System Git | Grit origin/main | Grit fixed |
| --- | ---: | ---: | ---: |
| 10k loose | 1,923 ms / 19.4 MiB | 1,525 ms / 55.8 MiB | 1,482 ms / 40.9 MiB |
| 20k loose | 3,793 ms / 33.9 MiB | 2,986 ms / 96.0 MiB | 2,940 ms / 61.8 MiB |
| 20k packed (daily) | 2,471 ms / 48.1 MiB | 2,832 ms / 107.5 MiB | 2,700 ms / 74.7 MiB |

Times are medians; RSS is the maximum of the four measured runs. Every result tree
matches system Git, the index and worktree remain clean, and `git fsck --strict` passes.
Raw samples and both baselines are committed in
[`merge-content-memory.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/merge-content-memory.json).

Separate Heaptrack profiles put peak live allocations at **57.20 → 37.54 MB** for
loose objects and **67.22 → 47.63 MB** for the packed fixture (decimal MB). The
original peak retained merge maps; after removing those copies, checkout's repeated
diff records became material and were changed to references. The RSS table excludes
profiler overhead.

Criterion's `merge_content` bench exercises the library tree merge without checkout:
**1,345.8 ms before → 1,292.8 ms after** (10 samples, about 4% faster). Run it with:

```bash
cargo bench -p grit-lib --bench merge_content
```

Linux regression tests measure tree merging and full integration in separate fresh
processes, including loose and packed objects. The measured debug API peaks were
about **56/58 MiB** for loose tree/full merges and **74/82 MiB** for packed tree/full
merges. Their budgets are **65 MiB loose** and **88 MiB packed**, allowing allocator
variation while rejecting the old loose merge's roughly 78/95 MiB peaks. Packed RSS
includes mapped pack pages and bounded object/delta caches. Git still uses less memory;
the regression budgets preserve the measured improvement.

## Local unshallow with historical tags

Issue [#2053](https://github.com/gitbutlerapp/grit/issues/2053) covers tags entering newly fetched history. Local fetch now selects following tags using the shallow response's effective boundaries. Lightweight targets arrive through the branch roots; missing annotated tag objects and chains get additional wants. Already-local tags still skip the reachability walk.

Factory VM, 2026-10-26: 4 vCPU AMD EPYC 9V74, 16 GiB RAM, Ubuntu 24.04, Rust 1.99 `release`, system Git 2.55.0. The packed `file://` fixture has 3,000 linear commits, 1,500 tags on even-numbered commits and 1,000 extra tip tags. Each sample starts from a fresh system-Git depth-1 clone outside the timed region. Complete no-op clients are first unshallowed by Git. One warmup precedes seven interleaved runs per binary, with rotating order and builds/tests paused. Wall time uses Python `perf_counter`; peak RSS uses `/usr/bin/time -v`. Automatic maintenance is disabled for all binaries.

| Operation | System Git | Grit main | Grit fixed |
| --- | ---: | ---: | ---: |
| One unshallow fetch | 226.7 ms / 8.6 MiB | 213.3 ms / 23.7 MiB | 492.0 ms / 25.8 MiB |
| Complete history and tags | 226.8 ms / 8.6 MiB | 537.6 ms / 23.3 MiB | 495.9 ms / 26.1 MiB |
| Complete no-op fetch | 51.8 ms / 7.0 MiB | 26.5 ms / 13.8 MiB | 25.9 ms / 15.3 MiB |
| Shallow no-op fetch | 26.9 ms / 6.1 MiB | 32.8 ms / 14.1 MiB | 32.3 ms / 15.6 MiB |

Times are medians; RSS is the largest peak across seven runs. Main is `e10563aae0fde79b96e13cca50d170d3d35e7b7b`. **Main's single unshallow result is incomplete:** it omits 1,499 historical tags. The complete-result row includes its required second fetch, including both commands' elapsed time and the larger process RSS. Git and fixed Grit produce all tags in one operation. No-op timing remains within the observed sample spread; following tags does not add a history scan when every tag is already local. RSS includes the tag updates retained during object copying in the fixed single-fetch operation. In ten separate complete no-op runs sampled through `/proc/<pid>/status`, median anonymous RSS stayed about 1.8 MiB before and after; the measured no-op RSS increase is in file-backed pages. All final results pass `git fsck --strict`.

Raw timing and RSS samples, fixture details and commands are recorded in [`fetch-local-unshallow-tags.json`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/fetch-local-unshallow-tags.json).

## Local fetch with many refs

Local fetch reuses the repository's ref store for tag checks, tracking-ref classification, and object-copy exclusion tips. Exclusion tips are collected only when the finalized shallow/deepen request needs an object copy. Up-to-date fetches still perform pruning and fetch metadata maintenance; loose refs override packed refs, symbolic refs resolve through the store, and both files and reftable backends retain their freshness checks.

Factory VM, 2026-10-26, 4 vCPU, Rust 1.99, `release` profile, system Git 2.55.0. The issue #2037 fixture contains one commit, 3,000 extra branches and 2,000 tags, with packed remote refs and a system-Git-cloned client. The deep-history fixture uses upstream `git/git` at `7e34d8174dcda3435d1404c9a30bef46afe6c13e` (82,563 commits), its original refs, and the same extra branches and tags. Every command fetches from a local bare repository without transferring objects or changing refs.

| Fixture / command | Git mean ms | Grit main mean ms | Grit after mean ms | Git peak KiB | Grit main peak KiB | Grit after peak KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| One commit, `fetch origin` | 41.3 | 219.1 | 35.5 | 9,024 | 17,744 | 16,456 |
| One commit, `fetch --no-tags origin` | 37.2 | 210.6 | 24.1 | 8,684 | 17,188 | 16,268 |
| Deep history, `fetch origin` | 47.9 | 219.6 | 41.0 | 43,844 | 19,696 | 19,080 |
| Deep history, `fetch --no-tags origin` | 40.7 | 203.7 | 24.7 | 43,664 | 19,504 | 19,016 |

The baseline is `origin/main` at `a61c6cf784d65e54c9f8105b0d8f66e0403dccee`. Frozen binaries for that revision and the working-tree fix were measured alongside stock Git in one session. Run `hyperfine -N --warmup 3 --runs 10` with those three binaries, first with `fetch origin`, then with `fetch --no-tags origin`, from each client. Builds and tests were paused for measurements. Peak RSS comes from separate `/usr/bin/time -v` runs; the table uses the larger of the two Git RSS samples. Individual wall-time samples, spread, and instrumentation are committed in `grit-utils/baselines/fetch-local-many-refs.json`.

Separate `strace -f -e trace=openat` runs counted all open attempts, including missing files, for paths ending in `/config` or `/packed-refs`. On the one-commit fixture, default Grit fetch went from **9,039 config opens and 3,008 packed-refs opens to 34 and 6**. Git used 35 and 4. With `--no-tags`, Grit went from 9,037 and 3,007 to 34 and 6. Cache validation and loose-ref lookup remain enabled.

The daily scenarios `fetch/noop-local@many-refs` and `fetch/noop-local-no-tags@many-refs` reproduce this shape with 20 alternating samples per tool, check unchanged refs and `git fsck --strict`, and require no cached network fixture. Their committed before/after daily reports include helper-inclusive RSS, which differs from the direct process measurements above. Reproduce them with:

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench --grit ./target/release/grit daily \
  --only fetch/noop-local@many-refs,fetch/noop-local-no-tags@many-refs \
  --output /tmp/fetch-local-many-refs.json
cargo bench -p grit-lib --bench fetch_local_many_refs
```

Criterion mean library times (20 samples, one-second warmup, three-second measurement target):

| Tag mode | Before ms | After ms |
| --- | ---: | ---: |
| Following | 215.43 | 31.64 |
| None | 203.20 | 20.73 |
| All | 354.65 | 47.79 |

The Criterion suite exercises the public `fetch_local` API with `Following`, `None`, and `All` tags on a system-Git-created 5,000-ref fixture. Compatibility tests also cover changed remote tips, loose overrides, symbolic refs, pruning followed by a no-op fetch, linked worktrees, reftable, and explicit `FETCH_HEAD` maintenance. CI checks behavior rather than elapsed-time thresholds.

## Bounded branch containment on 10,000 unique tips

Issue #2035 is measured on a full clone of git.git at `7e34d8174dcda3435d1404c9a30bef46afe6c13e` (82,563 reachable commits). Create 10,000 distinct `refs/heads/dogfood/NNNNN` tips from `git rev-list -n 10000 HEAD`, then run `git pack-refs --all`. This fixture has no commit-graph. The review correction was measured on 2026-10-26 against stock Git 2.55.0: Ubuntu 24.04.5, four vCPUs (AMD EPYC 7763), 16 GiB RAM, Rust 1.99.0, `release` profile. Main is `a61c6cf784d65`, reviewed is `9d66d522369c`, and corrected is `da77132c09dd`. No compiler or test suite runs during measurement.

```bash
grit branch --contains HEAD~100 -n 100 --json
git for-each-ref --contains=HEAD~100 --count=100 \
  '--format=%(refname) %(subject) %(committerdate:iso8601) %(upstream)' refs/heads/
# Human comparison: omit --json and use --format='%(refname:short)' for Git.
# Unlimited comparison: omit -n / --count; unfiltered control: omit --contains too.
```

A 100-row JSON query warms every executable before alternating measurements. Limits 1 and 100 and the unfiltered control use three alternating Git/main/reviewed/corrected samples; limit 1,000 and unlimited containment use one sample because Git scans the whole ref inventory. Wall time surrounds the subprocess, and peak RSS comes from `/usr/bin/time -v`. Ordered JSON and human refs and JSON truncation are checked against system Git. The before/reviewed/after baseline JSON files retain every timing sample, maximum RSS, and the measured commit in `tools.grit_commit`.

| Output / limit | Git ms / KiB | Main ms / KiB | Reviewed ms / KiB | Corrected ms / KiB |
| --- | ---: | ---: | ---: | ---: |
| JSON / 1 | 33.82 / 19,164 | 2608.18 / 100,924 | 14.74 / 26,996 | 14.30 / 27,120 |
| JSON / 100 | 199.07 / 31,312 | 2612.84 / 101,288 | 34.28 / 34,412 | 33.91 / 34,668 |
| JSON / 1000 | 34184.82 / 70,880 | 2630.10 / 100,904 | 249.73 / 59,900 | 232.96 / 60,352 |
| JSON / unlimited | 34139.51 / 70,872 | 2622.00 / 101,032 | 2366.62 / 102,608 | 2389.44 / 102,420 |
| JSON / unfiltered, unlimited | 158.39 / 25,604 | 182.69 / 37,100 | 180.51 / 38,560 | 180.89 / 38,644 |
| Human / 1 | 34.27 / 19,136 | 2637.82 / 101,036 | 14.82 / 26,992 | 14.73 / 26,932 |
| Human / 100 | 198.35 / 31,152 | 2608.95 / 102,092 | 33.59 / 34,020 | 33.54 / 34,288 |
| Human / 1000 | 34608.55 / 70,668 | 2636.97 / 102,468 | 227.79 / 59,976 | 229.86 / 60,240 |
| Human / unlimited | 34405.21 / 70,732 | 2625.78 / 102,072 | 2393.66 / 102,100 | 2352.27 / 102,412 |
| Human / unfiltered, unlimited | 161.70 / 5,240 | 135.36 / 19,816 | 134.94 / 19,688 | 134.11 / 19,720 |

The finite name-ordered path shares positive and negative ancestry between evaluated tips and stops after enough matches, including the CLI's extra truncation probe. Without a commit-graph, expensive negative probes can meet an incrementally explored boundary of proven target ancestors. Committer dates order that exploration but never exclude ancestry. Unlimited queries, version/date sorts and combined filters retain the batch path. Grafts and enabled replacement refs also use the batch path: rewritten parents can form cycles, so a target's ancestors are not necessarily nonmatches. Commit-graph linkage and generations are ignored for these rewritten histories.

The correction retains the reviewed build's bounded-query speed and memory use on ordinary histories; the unlimited and unfiltered controls remain comparable. Rewritten histories use the general batch path for correctness.

The original bounded-query implementation (`9d66d522369c`) measured these means on its synthetic 10,000-tip Criterion fixture (10 samples, 0.1-second warmup and 0.2-second requested measurement, extended automatically for long queries):

| Matching-row limit | No commit-graph ms | Commit-graph ms |
| --- | ---: | ---: |
| 1 | 6.02 | 6.18 |
| 100 | 7.10 | 6.86 |
| 1,000 | 64.78 | 31.16 |
| Unlimited | 284.22 | 184.35 |

Run the library fixture with `cargo bench -p grit-lib --bench ref_query_contains_tags`. Its `ref_query_contains_10k_unique_branches` group covers limits 1, 100, 1,000 and unlimited with and without a commit-graph, opening a fresh repository handle per sample. The daily command matrix includes `branch/contains@git-git-10k-refs` and `branch/contains-json@git-git-10k-refs`, using an isolated copy of the pinned git.git fixture.


## Linux worktree metadata scans (2026-10-26)

Status and worktree diffs used to construct an absolute path for each indexed file and repeat the lookup of its parent directories. On 64-bit Linux, larger scans now open each eligible parent once with `O_PATH` and inspect its file names with `fstatat` and `AT_SYMLINK_NOFOLLOW`. The untracked walk also reuses the operation's configuration instead of loading the cascade again to choose its worker settings. Index stat fields and file-type handling stay the same. Scans below 1,000 entries, directories below eight files, and case or Unicode path resolution keep the existing metadata lookup.

The [before](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/status-scan-before.json) and [after](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/status-scan-after.json) reports compare release builds based on `a61c6cf` with system Git 2.55.0 on a four-CPU AMD EPYC VM using ext4. Initial reproduction used `grit-bench daily --only status/clean@synth-10k` with 20 runs per tool. The final comparisons reuse the daily fixtures with warm indexes and rotate Git/before/after through `grit-bench measure-direct-run`: 30 runs per tool, 200 for small status, 10 for deep-tree, and 60 for mutating canaries. Both Grit binaries have the same basename and equal-length parent paths (`/tmp/grit-4658-binaries/{before,after_}/grit`); copied binaries were verified byte-for-byte against the builds. Times are medians; RSS is the maximum across the runs, cross-checked with ten `/usr/bin/time -v` runs per tool.

| Scenario | Git ms | Grit before ms | Grit after ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `status/clean@synth-10k` | 13.321 | 13.564 | 11.684 | 19744 | 19564 |
| `status/clean@small` | 1.670 | 2.831 | 2.778 | 15656 | 15480 |
| `status/clean@synth-100k` | 116.114 | 104.294 | 84.944 | 36104 | 35896 |
| `status/clean@deep-tree` | 2062.934 | 1024.723 | 1014.167 | 75236 | 75404 |
| `diff/worktree@synth-10k` | 11.841 | 10.873 | 9.061 | 19768 | 19496 |

Heaptrack on the 10k-file fixture counted **70,813 → 50,252 allocations** (29% fewer), and **31,001 → 11,662 temporary allocations** (62% fewer). Peak live heap stayed about 2.08 MB; process RSS remains broadly unchanged. The full reports include every related status case and the diff/staging canaries.

Related `diff`, `add`, and `commit` rows are included in the reports. The commit canary compares auto-staging `grit commit -m bench` with `git commit -am bench`; preparation and resets stay outside the timed region. The current daily `status/renamed` preparation resets to a clean tree, and `status/paths` selects `data/d0000`, absent from the current `synth-100k` fixture; those registry cases are reported as they currently run. Rust integration tests separately cover actual renames and raw-byte paths.

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

### Path-limited history

On 2026-10-26, release builds at upstream `818de8006` and the path-walk optimization were compared against system Git 2.55.0 on a four-vCPU AMD EPYC 7763 VM. The pinned git.git fixture is `v2.47.0`. Each tool had one warmup, then 10 alternating samples (30 for short commands) through the same `grit-bench measure-direct-run` helper used by the daily harness. The table reports median wall time and maximum peak RSS; GNU `time -v` independently checked the target.

| Scenario | Git ms | Grit before ms | Grit after ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `log/path@git-git` | 370.22 | 3229.01 | 462.62 | 312,212 | 273,720 |
| `log/limit-path@git-git` | 25.01 | 62.81 | 20.58 | 40,668 | 36,280 |
| `revs/count-path-head@git-git` | 453.37 | 2512.87 | 513.56 | 311,044 | 272,028 |

Path-limited history now scans the requested entries directly in reusable tree buffers, retains packed delta bases, and stops comparing merge parents once the selected path is unchanged. Commit-object caching remains available to author filters. The [full results](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/baselines/daily-log-path-4526.json) include all eleven log scenarios and related operations, with individual samples and the original build recorded as `baseline_grit`. The `revs/count-path-head` row uses pinned `HEAD`; the daily row’s historical revision `7e34d81` is absent from this fixture.

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

## Incremental push over SSH

The send-pack walk stops when no queued commit remains reachable only from the wanted tips. Advertised commits still propagate exclusion flags through shared ancestry; unrelated old refs no longer force a walk to the roots. Regression tests bound actual commit reads with a direct-parent have plus a near-root have, compare merge-history packs with system Git, and push through Git's receive-pack with annotated blob/tree tags advertised.

Factory VM, 2026-10-26, 4 vCPU, Rust 1.99, `release` profile, system Git 2.55.0: one new commit over a local fake-SSH wrapper on upstream `git/git` at `7e34d8174dcda3435d1404c9a30bef46afe6c13e` (82,563 commits on master). The source and bare receiver retain upstream refs and tags and use shared object storage without bitmaps. Before each run, reset the remote branch to the new commit's parent; push `HEAD:refs/heads/master` with each client. One warmup followed by six interleaved runs uses Python `perf_counter` wall time and `/usr/bin/time -f %M` peak RSS, with compilation paused. The committed samples are in `grit-utils/baselines/push-one-commit-ssh.json`.

| Client | Median wall time | Peak RSS |
| --- | ---: | ---: |
| System Git | 71.3 ms | 47.3 MiB |
| Grit before the cutoff fix (`c190a4fff`) | 2,629.2 ms | 74.6 MiB |
| Grit after | 144.7 ms | 51.9 MiB |

The cutoff reduces median push time by **18.2×**, with about **30% lower peak RSS**; Grit remains about **2.0×** slower than Git on this fixture. Every push updated the expected ref. `git fsck --strict --no-dangling` succeeded on the receiver, with warnings for historical upstream tag/tree objects. The small SSH regression fixture passes strict fsck without those historical objects.

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

## grit-lib: writing pack bitmaps

`grit gc repack --bitmaps` on an already-packed `rust-lang/cargo` clone (195,008 objects in one 81 MB pack, 24,341 commits, 110,673 trees, 109 release branches) used to spend about 56 s writing the `.bitmap` ([#1877](https://github.com/gitbutlerapp/grit/issues/1877)). The writer read every blob while walking trees, re-read commits inside sorts, walked full reachability a second time to check closure, and read the whole pack to get its 20-byte trailer. It now reads only commits and trees. Types come from pack entry headers, and an unwritten bitmap at each first-parent fork keeps release branches from re-walking the main line below the fork (tree reads went from 388k to about the number of distinct trees). Commit and type bitmaps are byte-identical to before; 66 of 195,008 name-hash entries changed.

4-vCPU VM, Git 2.43.0, wall time and peak RSS (`/usr/bin/time`), each on a fresh copy of the clone:

| Operation | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| `grit gc repack --bitmaps` vs `git repack -a -d -b` | 1.6 s / 174 MB | 58.3 s / 414 MB | 2.9 s / 414 MB |
| `grit repack` vs `git repack -a -d` (no bitmap, for reference) | 1.15 s / 169 MB | 2.0 s / 414 MB | 2.0–2.25 s / 414 MB |
| Criterion `pack_bitmap/write_from_git_bitmap` (bare cargo clone) | — | 54.87 s | 0.967 s |

The peak RSS comes from the repack itself, not from the bitmap write. Reproduce the library number with:

```bash
GRIT_GIT_GIT_BARE=/tmp/cargo.git cargo bench -p grit-lib --bench pack_bitmap -- write_from_git_bitmap
```

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

## Big files are streamed (2026-10-25)

`grit add` of a 1 GB file used to read it into memory and copy it twice more on the way to the object store, peaking at 2.4–3× the file size (issue #1850). Files of at least `core.bigFileThreshold` (512 MiB by default) that need no clean conversion are now hashed and deflated in 128 KiB chunks straight into their own pack, as Git does, at the pack compression level. Smaller files lost two copies (the clean conversion takes ownership of the bytes, and `add` deflates straight into the loose object file instead of a buffer kept until the batch finished), so they now peak at about 1× the file size, like Git.

Same Linux VM sitting (4 vCPU), release builds, system `git 2.43.0`, `/usr/bin/time -v`, one run each; grit is `origin/main` before → after:

| Command | Git | Grit before → after (wall) | Peak RSS Git, Grit before → after | `.git` size Git, Grit before → after |
| --- | ---: | ---: | ---: | ---: |
| `add` 1.09 GB text (`seq 1 120000000`) | 19.1 s | 5.3 → **7.4** s | 5 MB, 2,596 → **14** MB | 265, 466 → **271** MB |
| `commit` (auto-stage) 1 GB random | 22.4 s | 12.6 → **18.7** s | 5 MB, 2,997 → **16** MB | 1,000, 1,055 → **1,000** MB |
| `status` after touching the 1 GB file | 19.3 s | 1.9 → **0.8** s | 5 MB, 1,966 → **15** MB | |
| `add` 349 MB text | 3.0 s | 1.7 → **1.3** s | 346, 846 → **355** MB | 90, 155 → 155 MB |
| `add` 300 MB random | 6.3 s | 3.7 → **3.3** s | 298, 909 → **307** MB | 300, 316 → 316 MB |

Big files are slower than before because the pack level (`pack.compression`, default 6) replaces the loose level 1, whose zlib-rs fast path compresses text far worse. Both are still faster than Git, and the repository is now about Git's size. Set `pack.compression` (or `core.compression`) lower to trade size for speed. Files below the threshold keep the loose level, which is why their repository size is unchanged. `grit-bench add-initial` (20,000 small files) and `add` / `status` at 10,000 files are unchanged within noise.

`grit-bench add-big-file` (committed as `add-big-file.json`) times `add` of one untracked text file per size with the index and objects reset between runs, and records peak RSS:

```bash
./target/release/grit-bench --warmup 1 --min-runs 3 --format json --output grit-utils/baselines/add-big-file.json add-big-file --megabytes 256,1024
```

## Large-blob reads and restaging (2026-10-26)

Issue #2010 exposed copies of blob payloads in `cat`, unnecessary writes when
restaging unchanged content, and a local-clone statistics walk that inflated
blobs just to count them. Cat now uses the library's replacement-aware payload
stream. Restaging uses the same streaming hash threshold as status and stores
content only when its object id changes. Local transfers link loose objects,
and the statistics walk reads blob headers without loading their payloads.

Linux, 4 vCPUs, release builds, system Git 2.55.0, baseline `origin/main`
`c190a4fff`. The committed `large-blob-memory-before.json` and
`large-blob-memory-after.json` record `/usr/bin/time -v` wall time and peak RSS
for incompressible binary files. Cat, restore and clone use three runs with
rotating tool order on identical sources; the table reports medians. Status and
add are single runs after moving the file's mtime into the future without
changing its bytes. Every clone passes system `git fsck --strict` and a blob
hash comparison.

This capture predates the related #1849 integration. Current main also contains
the matching human-mode cat stream and loose-clone optimizations; this change
reuses that display API and keeps the restaging, structured-output bounds,
pack serialization and regression coverage described here.

After rebasing onto that integration, `grit cat HEAD:big.bin --json` on the
1 GiB packed fixture returned 173 bytes of metadata at 13.4 MiB peak RSS.
The RSS guard also covers JSON and Markdown output.

| Operation | Git seconds | Main seconds | After seconds | Git RSS MiB | Main RSS MiB | After RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Status, 128 MiB | 0.36 | 0.13 | 0.09 | 133.1 | 13.7 | 14.8 |
| Add, 128 MiB | 0.39 | 0.35 | 0.09 | 132.9 | 140.4 | 13.6 |
| Status, 1 GiB | 2.56 | 1.61 | 1.03 | 5.3 | 13.4 | 15.0 |
| Add, 1 GiB | 44.90 | 31.29 | 0.84 | 5.0 | 12.9 | 13.3 |
| Cat, loose 1 GiB | 0.41 | 1.12 | 0.21 | 1028.8 | 4449.7 | 12.7 |
| Restore, loose 1 GiB | 0.73 | 1.02 | 0.94 | 1029.1 | 14.3 | 14.4 |
| Clone, loose 1 GiB | 0.74 | 0.72 | 0.32 | 1029.5 | 2061.0 | 15.4 |
| Cat, packed 1 GiB | 0.37 | 1.33 | 0.23 | 1028.9 | 5474.2 | 1036.8 |
| Restore, packed 1 GiB | 0.69 | 0.99 | 0.94 | 1029.6 | 1038.4 | 1038.4 |
| Clone, packed 1 GiB | 0.71 | 0.38 | 0.31 | 1029.7 | 1039.7 | 1039.8 |

Linux RSS includes resident pages of memory-mapped object files, which explains
the roughly 1 GiB floor for packed reads here. Loose-object cat and clone now
stay near their fixed process overhead. Pack deltas still require reconstruction
in memory. Restore was already streaming on this Linux baseline; these results
do not claim validation of the original Windows-specific restore spike.

Reproduce the memory guard after building both helper binaries:

```bash
cargo build --release -p grit-cli -p grit-utils
GRIT_LARGE_BLOB_MEMORY=1 cargo test -p grit-lib --test large_blob_memory
```

To create the loose 1 GiB fixture, set `core.bigFileThreshold=2g` for the
system `git hash-object -w` command before building its tree and commit. Without
that override, Git stores the large file in a pack. Compare `git cat-file blob
<oid>` with `grit cat HEAD:big.bin`, and local `git clone` with `grit clone`;
redirect cat output to `/dev/null` and use a fresh clone destination each time.

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

## Status right after a checkout (2026-10-25)

A clone, switch or `reset --hard` writes every file and then the index within the same second, so every entry is racily clean. Under Git's default whole-second rule, the next status has to re-hash all of them once, and then rewrite the index. Git does this too. Dogfooding on Windows (issue #1914) measured that first `grit status` at 3–13 s on rust-lang/cargo, against 0.2 s for Git. Grit read the files one at a time, inside the serial pass over the stat scan, with a second `lstat` for each file, and on Windows every file open is expensive. Now `grit-lib` collects the racy entries from the scan, hashes them on the `IndexRefresh` worker pool, and applies the results from the scan's own stat data. The one-second rule is unchanged. A same-second edit with the same size is still caught by its content.

Same Linux VM sitting (4 vCPUs, ext4), release builds, system `git 2.43.0`. Each run starts from the index exactly as the clone left it. Times are hyperfine means in ms; "cold" drops the page cache before each run. Peak RSS comes from `/usr/bin/time`:

| Repository (tracked files, all racy) | Git | Grit before → after | Cold: Git, Grit before → after | Peak RSS Git, Grit before → after |
| --- | ---: | ---: | ---: | ---: |
| rust-lang/cargo (3,078) | 64.7 | 47.6 → **30.0** | 194.7, 178.3 → **106.6** | 8.4, 18.7 → 20.1 MiB |
| git/git (4,857) | 156.8 | 132.6 → **68.5** | 353.3, 325.4 → **164.2** | 7.7, 20.5 → 23.3 MiB |

After the first run, status on a clean tree is unchanged: 16.8 ms on cargo and 5.2 ms on git/git. On cargo, `grit switch --detach <old>` followed by `grit status` drops from 56 to 28 ms. The extra 1.5–3 MiB of peak memory belongs to the worker threads: with `index.threads=1`, peak RSS matches the old serial path. On Windows the parallel opens are where most of the gain comes from.

`grit-bench status` now includes a `status-after-checkout-{N}` row. Its hyperfine prepare step restores the racily clean index before each run. The committed results are `status-after-checkout-before.json` → `status-after-checkout-after.json`: `status-after-checkout-10000` goes from 50.2 to **27.9** ms (Git 31.6), and `status-after-checkout-1000` from 7.5 to **6.0** ms (Git 5.0). The dirty and clean rows are unchanged within noise.

```bash
./target/release/grit-bench --format json --output grit-utils/baselines/status-after-checkout-after.json status --sizes 1000,10000
```

## Adding many untracked directories (2026-10-25)

`grit add .` on a tree of thousands of untracked directories was quadratic in the directory count (issue #1854): 4,000 one-file directories took 90 s and 1 GB where `git add -A` takes 34 ms. Any pathspec, `.` included, made the untracked walk collapse each wholly untracked directory to `dir/` and then re-walk the whole work tree once per collapsed directory to list its files. Two changes in `grit-lib`:

- `add` treats `.` and `./` as no pathspec, and asks the walk for every untracked file in one pass. Collapsed `dir/` entries now only come from a normal-mode untracked cache on the index, which `add` reuses instead of rebuilding.
- Expanding collapsed directories (`expand_untracked_for_staging`) walks each one once, starting at the directory itself, and filters its files by the pathspecs. `stash -u` and the commit staging engine no longer expand at all, because their walks already list every file.

Same Linux VM sitting, release builds, system `git 2.43.0`, `/usr/bin/time` wall time and peak RSS, empty index (`git init` then add). Grit is `origin/main` before → after:

| Layout | Git | Grit before → after | Peak RSS Git, Grit before → after |
| --- | ---: | ---: | ---: |
| 1,000 one-file directories | 0.01 s | 4.97 → **0.02** s | 5, 79 → **16** MiB |
| 2,000 one-file directories | 0.02 s | 21.2 → **0.03** s | 5, 275 → **19** MiB |
| 4,000 one-file directories | 0.04 s | 90.5 → **0.06** s | 6, 1,059 → **23** MiB |
| 49,000 files in `d<i%200>/s<i%37>` | 0.29 s | 18.5 → **0.59** s | 14, 668 → **84** MiB |

With one tracked file committed first, `grit add 'd*'` on 2,000 directories went from 21.7 to 0.04 s, and `grit commit` (stage all) from 2.25 to 0.06 s. The staged index is byte-identical to `git add -A` (`git ls-files -s`). `grit-bench add-many-dirs` records the 4,000-directory and 49,000-file rows in `add-many-dirs.json`:

```bash
./target/release/grit-bench --format json --output grit-utils/baselines/add-many-dirs.json add-many-dirs --dirs 4000 --nested-files 49000
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

## Reset reuses the index it replaces (2026-10-25)

The daily census had `reset/mixed@synth-10k` (`grit reset HEAD` on a tree with 1% of files edited) at 5× Git. A mixed reset built the new index from the target tree with empty stat data, so the refresh that followed opened and hashed all 10 000 tracked files (`strace`: 10 055 `openat`, 20 051 `read`, 30 358 `statx`) where Git only stats them.

- **Mixed reset seeds the new index from the current one.** Rows whose path, mode and blob id are unchanged keep their cached stat data, so the refresh hashes only files whose stat changed (156 `openat`, 251 `read`, one `statx` per file).
- **Unchanged subtrees come from the cache-tree, not the object database.** Like Git's one-way merge, flattening the target tree copies the current index's rows for every directory whose cache-tree node is valid and names the same tree, after checking that the node's entry count covers exactly the plain rows under that directory, so a stale node is never trusted. When the whole tree matches (`reset HEAD`), the rows are reused in place. Partial commits (`commit -- <paths>`) and `stash push` build their seeded index the same way.
- **`reset --hard` flattens the target tree the same way and keeps the cache-tree.** It used to read every tree object on every run and then write the index without a `TREE` extension, which Git primes after a hard reset. Without it, the next command had nothing to reuse.

Same VM sitting, release builds, system `git 2.43.0`, `origin/main` before → after (`grit-bench daily --min-runs 10`, committed in **`daily-reset-before.json`** and **`daily-reset-after.json`**):

| Scenario | Git ms (before / after run) | Grit ms before → after | Grit ÷ Git before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: | ---: |
| `reset/mixed@synth-10k` | 8.8 / 9.1 | 45.6 → **12.2** | 5.17× → **1.35×** | 20.6 → **19.6** MiB |
| `reset/hard@synth-10k` | 16.4 / 16.5 | 21.8 → **20.1** | 1.33× → **1.22×** | 22.7 → 22.7 MiB |
| `reset/hard-delete-heavy@wide-48k` | 240.9 / 237.1 | 236.9 → 227.9 | 0.98× → 0.96× | 50.0 → 49.3 MiB |
| `commit/paths@synth-10k` | 22.8 / 22.7 | 28.6 → **25.5** | 1.25× → **1.12×** | 25.8 → 25.9 MiB |
| `stash/push-pop@synth-10k` | 213.7 / 212.7 | 158.6 → 148.4 | 0.74× → 0.70× | 26.7 → 26.3 MiB |

The mixed-reset samples spread from 10.2 to 14.5 ms in that run. Repeated with `hyperfine` on the dirty fixture without re-preparing, it is 43.6 → 9.9 ms (Git 7.9 ms). `status`, `switch`, `restore` and the other `commit` rows are unchanged within noise. The mixed reset's heap peak dropped from 3.9 to 2.8 MB (massif). Most of the remaining RSS gap to Git is the `grit` binary's mapped code pages (about 10.6 MB of the 16.7 MB resident at exit), not reset data.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench daily \
  --only reset/mixed@synth-10k,reset/hard@synth-10k,reset/hard-delete-heavy@wide-48k,commit/paths@synth-10k,commit/small@synth-10k,commit/dry-run@synth-10k,stash/push-pop@synth-10k,status/dirty@synth-10k,restore/paths@synth-10k,switch/branch@synth-10k \
  --min-runs 10 --output grit-utils/baselines/daily-reset-after.json
```

## Content merges on both sides (2026-10-25)

A merge where both branches edited the same files grew with the number of content-merged paths times the size of the index: 146 s for 20 000 files edited on both sides, where Git takes 2.9 s, and 112 s for 2000 overlapping edits in a 200 000-file tree (issue #1881). For every path that needed a three-way content merge, the tree merge reloaded the index and rebuilt the attribute and ignore rules (config, `info/exclude`, the global ignore file, `.gitattributes`) up to four times, just to look up `merge=union`, `-merge`, `-text` and `-diff`. The rules are now built from one index load per tree merge, on the first path that needs them, and each path looks its attributes up once. Results are unchanged: the merged trees are identical to Git's.

Same VM sitting, release builds, system `git 2.43.0`, `/usr/bin/time` wall time and peak RSS, one run each:

| Repository | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| 2000 files, all edited on both sides | 0.36 s / 9 MB | 2.26 s / 25 MB | **0.24 s** / 25 MB |
| 20 000 files, all edited on both sides | 2.85 s / 40 MB | 145.9 s / 100 MB | **2.66 s** / 103 MB |
| 200 000 files, 2000 overlapping edits | 2.51 s / 96 MB | 112.2 s / 122 MB | **1.05 s** / 102 MB |

The new `grit-bench daily` row `merge/content-both-sides@wide-20k` (200 directories of 100 twenty-line files, `topic` appends a line to every file, `main` prepends one, packed fixture) is committed in **`daily-merge-content-wide-before.json`** and **`daily-merge-content-wide-after.json`**:

| Scenario | Git ms (before / after run) | Grit ms before → after | Grit ÷ Git before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: | ---: |
| `merge/content-both-sides@wide-20k` | 1395.6 / 1378.3 | 149 610.9 → **1236.5** | 107.2× → **0.90×** | 115 → 116 MB |

Peak memory is still about twice Git's on this shape and is not addressed by this change.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench daily --only merge/content-both-sides@wide-20k \
  --min-runs 10 --output grit-utils/baselines/daily-merge-content-wide-after.json
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

## Incremental fetch negotiation (2026-10-25)

An incremental `grit fetch` or `grit pull` of a few new commits into a `git/git` clone took about 740 ms against 115 ms for Git. Nearly all of it was negotiation: before sending its first `have`, a protocol v2 fetch walked the client's entire history, inflating every commit from its pack, only to send the first 16 commits and find the server ready. Now [`Remote::fetch`](rustdoc:grit_lib::remote::Remote) walks history lazily, one round at a time. It sends 16 haves, then twice as many per round. Each round repeats the commits the server acknowledged, because a v2 server keeps no state between requests. It stops when the server is ready, the haves run out, or 256 haves go unacknowledged after a first acknowledgement. Negotiation also starts from remote-tracking refs, as Git's does, so commits already held under `refs/remotes/` are not downloaded again. For a `git/git` clone whose branch was 100 commits behind its remote-tracking ref, that cut the pack for 5 new commits from 105 commits to 5.

`grit-bench daily` rows (10 runs per side, release build), `origin/main` before → after in one sitting against system `git 2.43.0` on the factory VM (4 vCPU), committed in **`daily-fetch-pull-git-git-before.json`** and **`daily-fetch-pull-git-git-after.json`**. The `git/git` remote is served by `git http-backend`, and `fetch/incremental-http` by `grit-http-server`:

| Scenario | Git ms | Grit ms before → after | Grit ÷ Git before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: | ---: |
| `pull/check@git-git` | 116.5 | 739.9 → **52.9** | 6.25× → **0.45×** | 67.3 → **50.1 MB** |
| `pull/fast-forward@git-git` | 129.6 | 729.9 → **57.6** | 5.94× → **0.44×** | 66.6 → **50.1 MB** |
| `fetch/incremental@git-git` | 114.9 | 729.6 → **52.0** | 6.29× → **0.45×** | 66.6 → **49.9 MB** |
| `fetch/one-branch@git-git` | 116.3 | 740.3 → **50.8** | 6.13× → **0.44×** | 66.7 → **49.8 MB** |
| `fetch/dry-run@git-git` | 113.0 | 732.2 → **49.5** | 6.25× → **0.44×** | 67.3 → **49.7 MB** |
| `fetch/incremental-http@git-git` | 16 069 | 15 583 → **13 513** | 0.97× → **0.84×** | 66.9 → **67.2 MB** |
| `fetch/noop@git-git` | 25.7 | 11.0 → **12.1** | 0.45× → **0.47×** | 17.3 → **17.8 MB** |
| `fetch/deepen@git-git` | 18 070 | 16 229 → **16 182** | 0.86× → **0.90×** | 222.7 → **228.7 MB** |
| `pull/rebase@synth-10k` | 76.4 | 27.5 → **27.4** | 0.36× → **0.36×** | 43.6 → **44.5 MB** |
| `clone/http@synth-10k` | 856.1 | 743.9 → **746.5** | 0.91× → **0.87×** | 30.3 → **31.3 MB** |

The rows that did not negotiate a deep local history are unchanged within noise: a 30-run re-measurement of `fetch/noop` gave 11.99 → 11.95 ms. The `fetch_v2_negotiation_rounds` integration test runs against `git daemon`. It fails if a fetch the server can answer after one round reads deep local history, if a later round drops the acknowledged commits, or if commits already held under `refs/remotes/` are sent again.

```bash
cargo build --release -p grit-cli -p grit-utils -p grit-http-server
./target/release/grit-bench daily --only pull/check@git-git,pull/fast-forward@git-git,fetch/incremental@git-git \
  --min-runs 10 --http-server ./target/release/grit-http-server --format json --output /tmp/fetch-pull.json
cargo test -p grit-lib --test fetch_v2_negotiation_rounds
```

## Receiving a pack: clone and fetch memory (2026-10-25)

Cloning over HTTPS peaked at several times the received pack's size ([#1672](https://github.com/gitbutlerapp/grit/issues/1672)). The received pack is spooled to disk and indexed through a memory map, and peak RSS counts the map's resident pages. The trailer checksum, the object scan and delta resolution each read the whole map, so the full pack stayed resident while it was indexed. After indexing, a check for thin-pack bases re-inflated every object again on one thread. Completing a thin pack then read the pack into memory and indexed it a second time.

Indexing now keeps the pack's resident pages within a few megabytes. The two sequential passes drop the map's pages every 2 MiB, and delta resolution reads each compressed delta from the file with a positioned read, as `git index-pack` does. The index build reports the thin-pack bases it took from the object database. Missing bases are appended to the pack file in place, the header count and checksum are rewritten, and records for the new objects are added to the index without rebuilding it. Smaller buffers also cut working memory: deltas waiting to be resolved take 32 bytes instead of 72, worker threads hand their records over in batches, and the `.idx` is sorted in place and streamed to disk. The thin-pack rewrite also fixes a compatibility bug. Before it, the `.idx` kept the old pack checksum, and `git fsck` reported "packfile does not match index" after an incremental `grit fetch` from GitHub.

Factory VM (4 vCPU), system `git 2.43.0`, release builds, `origin/main` (`2657344c7`) against this change in one sitting, median of three runs. Clone rows are `grit clone` / `git clone -q` from GitHub. Ingest rows index the pack that `git clone` received: [`install_pack_path`](rustdoc:grit_lib::objects::index_pack::install_pack_path) with fetch options against `git index-pack` on the same file.

| Scenario | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| clone `rust-lang/cargo` (78 MB pack) | 4.54 s / 46.6 MB | 3.42 s / 126.9 MB | **2.40 s / 72.4 MB** |
| clone `git/git` (326 MB pack) | 18.38 s / 96.8 MB | 10.69 s / 419.2 MB | **7.47 s / 167.4 MB** |
| ingest `rust-lang/cargo` pack | 3.06 s / 28.5 MB | 2.39 s / 117.3 MB | **1.28 s / 35.1 MB** |
| ingest `git/git` pack | 13.26 s / 63.5 MB | 8.75 s / 406.6 MB | **5.21 s / 77.4 MB** |

Ingest now peaks at 1.2× `git index-pack`, down from 4–6×, and runs in 40% less time. The rest of the clone peak comes from checkout, which reads blobs through the 96 MB delta-base cache (`core.deltaBaseCacheLimit`, the same default as Git).

Two gates keep the memory from regressing. The `pack_ingest_memory` integration test builds a 43 MB, 70k-object pack with `git fast-import` and fails when grit's ingest peaks above 1.5× `git index-pack` on it, each measured in its own child process. `origin/main` measures 5.7× there and this change measures 1.4×. The daily matrix row `clone/full@git-git` records time and peak RSS for a full `git/git` clone over smart HTTP from `git http-backend`. The `grit-bench network` clone, fetch, push and ls-remote scenarios now record peak RSS for both tools as well.

```bash
cargo test -p grit-lib --test pack_ingest_memory -- --nocapture
GRIT_CARGO_PACK=/path/to/received.pack GRIT_SKIP_VERIFY=1 \
  cargo test --release -p grit-lib --test pack_ingest_roundtrip rss_harness_install_pack_path -- --ignored --exact
```

## Large files in the work tree (2026-10-25)

Writing one 350 MB file to the work tree took grit 8–9 s and about 1 GB of memory on `restore`, `switch` and `clone`, where Git took about a second and 93 MB ([#1849](https://github.com/gitbutlerapp/grit/issues/1849)). Three costs added up:

- **Loose inflate was quadratic.** The loose-object reader handed zlib the whole spare capacity of its output buffer on every call. The zlib binding zero-fills that space each time, and with one 64 KiB input chunk per call that meant re-zeroing hundreds of megabytes per chunk. The buffer is now grown and zeroed only ahead of the inflater, and it is sized straight to the size the object header declares.
- **The blob was copied three times.** The loose store copied the payload out of the inflate buffer. The work-tree conversion copied it again even when it changed nothing. Restore then re-read and copied the written file to check it.
- **Nothing streamed.** `Odb::open_stream` read every stream to the end before returning it.

Checkout now streams a regular file larger than 1 MiB straight from the object database into the work tree when no conversion applies to its path. That means no `ident`, `working-tree-encoding`, smudge filter or LF-to-CRLF setting; see [`worktree_conversion_is_identity`](rustdoc:grit_lib::worktree::crlf::worktree_conversion_is_identity). Loose objects and objects stored whole in a pack are inflated as the file is written ([`Odb::with_stream`](rustdoc:grit_lib::objects::odb::Odb::with_stream)). Pack deltas are still rebuilt in memory, as in Git. Files at or under 1 MiB, which is nearly all of them, are still read with one lookup: a loose object's header, or a whole pack entry's header, gives the size before the body is inflated, so the size check is part of the read. A file that does stream is looked up twice, once to size it and once to open the stream, and a whole pack entry's bytes are checked against the pack index CRC before streaming, so a corrupt copy is never streamed. Streamed data goes to a temporary file next to the destination, which is renamed into place only after the object's size and zlib checksum have been verified. A corrupt object fails the checkout and leaves the old file, or no file, in place. Hashing a work-tree file (the racy-clean checks in `switch`, `restore` and `status`) no longer copies files whose content goes into Git unchanged, and hashes files of 1 MiB or more in chunks.

Factory VM (4 vCPU), system `git 2.43.0`, release builds, `origin/main` (`5d5136859`) against this change in one sitting, `/usr/bin/time` wall time and peak RSS. The repository is the issue's: one 349 MB text file (`seq 1 40000000`) committed loose.

| Command | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| `restore big.txt` | 0.95 s / 91 MB | 9.33 s / 1011 MB | **0.78 s / 14 MB** |
| `switch main` (writes the file) | 0.94 s / 91 MB | 8.21 s / 1012 MB | **0.48 s / 15 MB** |
| `restore` of 300 MB random bytes, packed | 0.43 s / 605 MB | 1.53 s / 1213 MB | **0.45 s / 314 MB** |
| `clone` of a packed repository with both files | 1.59 s / 689 MB | 2.12 s / 1963 MB | **0.72 s / 399 MB** |

In the packed rows peak RSS includes the pages of the mapped pack read through while inflating, and the time includes checking the pack entry against its index CRC before it is streamed. Git reads a packed blob under `core.bigFileThreshold` (512 MiB) into memory whole.

The daily matrix row `switch/large-blob@big-asset` switches away from a 300 MB loose text file and back, so each run checks the racily clean file before deleting it and then writes it again. Committed in **`daily-large-blob-before.json`** and **`daily-large-blob-after.json`**:

| Scenario | Git ms (before / after run) | Grit ms before → after | Grit ÷ Git before → after | Grit peak RSS before → after |
| --- | ---: | ---: | ---: | ---: |
| `switch/large-blob@big-asset` | 870 / 905 | 4691 → **615** | 5.39× → **0.68×** | 915 → **16** MiB |

Checkouts of many small files did not change: switching both ways between two commits that differ in every file of a 30 000-file tree measured 1.195 s → 1.188 s packed and 1.167 s → 1.107 s loose (hyperfine, 20 runs), and cloning that repository 564 → 543 ms.

Local clones now link loose objects, or copy their compressed files when linking is unavailable, before considering a new pack. Their transfer-statistics walk reads large blobs' headers instead of inflating their bodies: blobs have no outgoing links to traverse. This removes the remaining allocation proportional to a loose blob's size during clone. Human `grit cat` output also streams blobs of at least 1 MiB through the library's replace-aware display API; JSON and Markdown retain their content fields.

Follow-up measurements on 2026-10-26 used an AMD EPYC 7763 VM (4 vCPU), system Git 2.55.0, and identical `dist` profiles for current main (`a1f4e2168`) and this change (`c858ca944`). Builds and tests finished before timing began. The fixture is the issue's `seq 1 40000000` (348,888,897 bytes), stored loose. Each large scenario ran three times; the small-file clone ran seven times. Times are medians and RSS is the maximum across runs, measured with `/usr/bin/time -v`. Clone results pass system `git fsck` and have a clean `git status`; restored and switched files pass `git diff --exit-code`.

| Command | Git seconds / MiB | Grit main seconds / MiB | Grit after seconds / MiB |
| --- | ---: | ---: | ---: |
| `restore big.txt` | 0.88 / 90.8 | 0.77 / 11.2 | **0.76 / 11.2** |
| `switch main` (writes the file) | 0.88 / 91.0 | 0.50 / 12.1 | **0.50 / 12.4** |
| Local `clone` | 0.88 / 91.2 | 1.01 / 676.2 | **0.50 / 13.4** |
| `cat HEAD:big.txt` (Git: `cat-file blob`) | 0.74 / 90.5 | 0.70 / 1340.1 | **0.54 / 9.9** |
| Local clone, 5,000 small files | 0.37 / 6.1 | 0.43 / 18.0 | **0.42 / 18.0** |

Raw samples, including current main in each scenario's `baseline_grit` field, are committed in `grit-utils/baselines/large-blob-materialization.json`. The `clone_large_blob_rss` integration test clones a 64 MiB loose blob through the public library API, checks that it is linked, runs system Git's fsck/status checks, and limits peak-RSS growth to 16 MiB. Before the statistics-walk fix it grew by 128 MiB and failed; afterward it passes.

The `checkout_large_blob_rss` integration test restores and switches to a 64 MiB blob, loose and packed, and fails if the process's peak RSS grows by 16 MiB or more. On the original pre-streaming baseline (`5d5136859`) it grows by 139 MiB. Packed-object RSS also includes resident pages of the compressed pack mapping; this can make the strict RSS-growth assertion sensitive to the process's earlier high-water mark.

```bash
cargo test -p grit-lib --test checkout_large_blob_rss --test checkout_large_blob_streaming
./target/release/grit-bench daily --only 'switch/large-blob@big-asset' \
  --format json --output grit-utils/baselines/daily-large-blob-after.json --grit ./target/release/grit
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

Hyperfine medians (interleaved runs) of `dist` builds of `origin/main` (`fce0b9c5f`) and of this change rebased onto it, on the pinned git.git clone (factory VM, same sitting), with median peak RSS over 15 runs. Sub-5 ms rows shift by a few tenths of a millisecond with machine load, so compare the two grit columns, which were timed together:

| Command | git ms | grit main | grit after | git RSS | grit RSS main | grit RSS after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `log --limit 100 --json` (`log/json-100@git-git`) | 2.95 | 4.44 | 4.33 | 12.3 MiB | 18.8 MiB | 18.7 MiB |
| `log --limit 100` (`log/oneline-100@git-git`) | 3.03 | 3.97 | 3.91 | 12.2 MiB | 18.2 MiB | 18.2 MiB |
| `log --limit 50 --json --filter …` (`log/json-filter@git-git`) | 2.23 | 4.57 | 4.42 | 9.9 MiB | 17.2 MiB | 17.3 MiB |
| `log --limit 1000 --json` | 11.98 | 22.07 | 16.34 | 15.0 MiB | 23.5 MiB | 23.6 MiB |
| `log --limit 5000` (`log/all@git-git`) | 47.79 | 106.00 | 64.28 | 18.0 MiB | 31.4 MiB | 31.4 MiB |

On the headline row, five interleaved 200-run rounds put the change 1.0–5.5% ahead of `main` every time (median 4.43–4.49 ms → 4.23–4.37 ms), and `perf stat` task-clock over 200 runs drops from 3.84–3.87 ms to 3.69–3.78 ms. Path-limited, `--author`, `--stat`, `revs` and `show` rows are unchanged within noise in interleaved runs (`daily-log-git-git-before.json` / `daily-log-git-git-after.json` hold the full daily matrix capture for both binaries). What remains of the 100-commit gap is mostly fixed cost: argument parsing, repository discovery and config loading, and opening the pack for the first object.

The daily runner used to read the clock only when a 1 ms `try_wait` poll first saw the child gone, so every sample was rounded up to a whole poll tick (about 1.1 ms here): sub-5 ms rows could only report values like 3.29, 4.39 or 5.44 ms, and a few-tenths gain stayed invisible unless it crossed a tick. On Linux it now blocks in `waitid` and reads the clock when the child exits, while a watchdog thread keeps the RSS sampling and the timeout and memory caps. Sub-5 ms rows captured before that fix (for example older `daily/` files) carry that rounding. Pass `--baseline-grit` to time a second grit binary in the same run for paired headline comparisons:

```bash
cargo build --profile dist -p grit-cli -p grit-utils
./target/dist/grit-bench daily --only 'log/json-100@git-git,log/oneline-100@git-git' \
  --grit ./target/dist/grit --baseline-grit /path/to/main/grit --min-runs 20 --format json --output /tmp/log-paired.json
./target/dist/grit-bench daily --only 'log/json-100@git-git,log/oneline-100@git-git,log/all@git-git,log/author@git-git,log/json-filter@git-git,log/limit-path@git-git,log/path@git-git,log/stat-n50@git-git' \
  --min-runs 30 --format json --output grit-utils/baselines/daily-log-git-git-after.json --grit ./target/dist/grit
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

#### Full repack of a fresh clone

The maintenance suite also runs **`repack-cargo-shape`** (`grit repack` vs `git repack -ad`) and **`repack-force-cargo-shape`** (`grit repack --force` vs `git repack -adf`) on a synthetic cargo-shaped repository (`CargoShape::BENCH` in `grit-utils/src/repack_fixture.rs`: 8 000 commits over a nested crate layout with hot `Cargo.lock` / `CHANGELOG.md` files, imported with `git fast-import` and packed with `git repack -adf`, like a fresh clone).

Default repack copies stored entries, both whole objects and deltas with their base reference rewritten, and runs the delta search only on objects that are not already deltas against a packed base. `--force` searches every object in a streaming window. Both write the pack to a staging file. Measured with `/usr/bin/time` on [rust-lang/cargo](https://github.com/rust-lang/cargo) (23 565 commits, one 78 MB pack after `grit clone`, 4-vCPU Linux VM, git 2.43.0):

| Command | Before | After | `git` |
| --- | --- | --- | --- |
| `grit repack` vs `git repack -ad` | 2.5–4.0 s, 412 MB, 81.5 MB pack | 1.24–1.35 s, 194–197 MB, 81.4 MB pack | 1.32–1.41 s, 169 MB, 81.4 MB pack |
| `grit repack --force` vs `git repack -adf` | 393 s, 5.5 GB, 107 MB pack | 7.3–8.9 s, 459–480 MB, 45.7 MB pack | 6.9–7.0 s, 470–476 MB, 45.2 MB pack |
| `grit gc` vs `git gc` (already packed) | 0.33–0.37 s, 42 MB | 0.36–0.46 s, 42 MB | 1.53 s, 169 MB |

Before is the parent `main` commit; its forced row is that library's `force_recompress` path, which had no CLI flag yet.

The gate keeps this from regressing: `grit-utils/tests/repack_cargo_shape_budget.rs` builds `CargoShape::SMOKE` and requires both repack modes to stay within **2×** of git's peak memory (measured above each tool's `log -n 1` baseline, so debug-build code pages do not count) and, in optimized builds, within **2×** of git's time, with a pack no larger than **1.2×** git's. `grit-lib/tests/repack_delta_reuse_git_compat.rs` checks against the system `git` that default repack keeps every stored delta, `--force` stays within 1.2× of `git repack -adf`, and chains deeper than `pack.depth` are cut.

#### Repositories full of loose objects

Baseline **`grit-utils/baselines/gc-loose.json`** (`grit-bench gc-loose`) times full **`grit gc`** against **`git gc`**, with peak RSS, on repositories whose objects are all loose, restored from a pristine copy before every run (**`prepare-gc-loose`**). **`gc-loose-history`** is the deep-history network fixture with its pack exploded by `git unpack-objects`, so the delta search runs over every blob, tree and commit. **`gc-loose-wide`** is one commit of 200 000 small files (no delta work, many objects).

The same shape used to break the pack-objects path that serves fetches and clones and builds pushes and bundles ([#1883](https://github.com/gitbutlerapp/grit/issues/1883)). That path loaded every loose payload, ran a second 50-deep same-path pass and only deltified blobs. It now hands blobs, trees and commits to the same streaming window search as repack, with commits walked in date order and subtrees named by path. On rust-lang/cargo with its pack exploded into 194 636 loose objects (4 vCPU, git 2.43.0, `/usr/bin/time`):

| Command | Before | After | `git` |
| --- | --- | --- | --- |
| serve a full clone (`upload-pack`) | 195 s, 5.2 GB, 99.5 MB pack | 9.2 s, 207 MB, 39.9 MB pack | 17.0 s, 97 MB, 39.4 MB pack |
| `grit clone file://` vs `git clone file://` | 391 s, 5.2 GB, 102.5 MB pack | 11.5 s, 228 MB, 45.5 MB pack | 18.2 s, 95 MB, 39.3 MB pack |
| serve a full clone of the packed repository | 2.97 s, 360 MB | 2.63 s, 289 MB | 1.88 s, 166 MB |

`grit-lib/tests/pack_objects_loose_git_compat.rs` checks against the system `git` that a pack built from an all-loose history indexes under `git index-pack --strict`, deltifies most trees, and stays within **1.2×** of `git repack -adf`.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench gc-loose --format json --output grit-utils/baselines/gc-loose.json
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

**2026-10-25 status:** Pack verification now streams each pack's delta trees depth-first on worker threads, checks every object as soon as it is inflated, and keeps only compact per-object facts; the connectivity walk re-reads trees instead of caching every entry. The committed baseline is **0.82×** git (~**156 ms** vs ~**191 ms** median, peak RSS ~**76 MiB** vs ~**42 MiB**), down from ~**9.4×** (~**1840 ms**). On full clones measured with `/usr/bin/time` on the factory VM:

| Repository | git fsck | grit fsck before | grit fsck now |
| --- | --- | --- | --- |
| rust-lang/cargo (195k objects, 80 MB pack) | 6.0 s, 232 MB | 3.5 s, 2.45 GB | **1.6 s, 277 MB** |
| git/git (422k objects, 322 MB pack) | 34.2 s, 513 MB | 74.3 s, 13.0 GB | **17.7 s, 638 MB** |

**Memory guard:** `grit-bench fsck` fails when grit's peak RSS exceeds both **2×** git's and git's plus **64 MiB** (the slack covers fixed per-thread overhead on small fixtures). The Rust test `grit-lib/tests/fsck_memory.rs` checks that fsck's peak does not grow with the uncompressed size of a delta-heavy pack.

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench fsck --format json --output grit-utils/baselines/fsck-medium.json
```

## Checkout cancellation batches (2026-10-26)

Checkout now decides whether to use its worker pool before splitting planned jobs into cancellation batches. Batches grow from eight to 2,048 files, with a separate 1 MiB prepared-content threshold and bounded parallel read-ahead. Large files still stream; each write flush contains at most eight streams. Small loose-object cache reuse is limited to bulk checkout, while direct writes and a one-pass object walk avoid that bookkeeping.

On the factory VM (AMD EPYC 7763, four available logical CPUs, ext4, Rust 1.99.0, system Git 2.55.0), the six regression scenarios improve **13–25%** from main snapshot `12b52a55c` to `caf6f64fa`. The table reports median wall time and maximum child RSS, using release builds, one warmup, and 12 timed runs per tool for the shared checkout cases; additional daily A/B cases use ten runs per tool. All six orders are cycled so trial predecessors are balanced, including boundaries. Additional A/B rows show their after-session Git control; both Git controls are retained in the data. Small differences around 1 ms should be read against the identical-binary control spread.

| Scenario | Git ms | Grit before ms | Grit after ms | Peak RSS KiB before / after |
| --- | ---: | ---: | ---: | ---: |
| `switch/branch@synth-10k` | 46.9 | 33.4 | 33.6 | 22,824 / 22,736 |
| `restore/paths@synth-10k` | 21.2 | 21.5 | 21.7 | 22,572 / 22,652 |
| `reset/hard@synth-10k` | 42.6 | 45.6 | 45.0 | 25,196 / 25,208 |
| `worktree/add@synth-10k` | 325.3 | 426.0 | 320.7 | 26,164 / 27,124 |
| `merge/clean@synth-10k` | 169.0 | 184.1 | 145.6 | 28,148 / 27,400 |
| `pick/one@synth-10k` | 182.5 | 185.1 | 161.8 | 27,620 / 27,992 |
| `switch/branch@synth-100k` | 241.6 | 107.3 | 105.5 | 54,692 / 54,816 |
| `switch/delete-heavy@wide-48k` | 227.6 | 207.3 | 196.3 | 35,376 / 35,784 |
| `switch/large-blob@big-asset` | 843.2 | 478.6 | 474.2 | 18,648 / 18,508 |
| `merge/clean@synth-100k` | 458.2 | 274.7 | 234.8 | 59,492 / 60,240 |
| `revert/one@synth-10k` | 182.6 | 194.7 | 158.3 | 28,176 / 28,240 |
| `pick/one@synth-100k` | 285.5 | 347.1 | 296.4 | 58,140 / 58,496 |
| `clone/checkout@synth-100k` | 7077.6 | 5933.7 | 4473.5 | 79,072 / 81,128 |
| `merge/content-both-sides@wide-20k` | 2113.5 | 3020.0 | 2679.7 | 112,440 / 114,052 |
| `clone/local@synth-10k` | 1052.4 | 1364.6 | 1264.7 | 31,172 / 31,804 |
| `clone/http@synth-10k` | 1139.6 | 1415.9 | 1308.9 | 31,964 / 32,260 |
| `clone/bare@synth-10k` | 711.4 | 795.3 | 796.8 | 24,300 / 24,680 |
| `clone/shallow-depth1@synth-10k` | 412.5 | 504.4 | 398.2 | 27,804 / 28,176 |
| `clone/shallow-depth1-http@synth-10k` | 422.6 | 514.6 | 408.7 | 27,600 / 28,176 |
| `worktree/remove@synth-10k` | 204.1 | 174.1 | 174.6 | 13,620 / 13,496 |
| `worktree/list@synth-10k` | 2.9 | 3.7 | 3.6 | 12,648 / 12,752 |
| `reset/hard-delete-heavy@wide-48k` | 380.8 | 421.3 | 410.3 | 59,296 / 62,828 |
| `reset/mixed@synth-10k` | 15.7 | 18.9 | 18.7 | 20,208 / 20,480 |
| `clone/full@git-git` | 20627.0 | 10821.3 | 10643.9 | 185,648 / 184,988 |
| `clone/shallow@git-git` | 2147.0 | 2312.6 | 2241.0 | 73,308 / 72,148 |
| `clone/blobless@git-git` | 12120.1 | 9740.9 | 9656.4 | 93,232 / 88,988 |

Memory guards cover 256 distinct 512 KiB blobs under serial and parallel preparation and an already-cached 64 MiB blob. Prepared-buffer growth stays below 16 MiB; checking cached size before copying reduces the cached-large-blob guard's additional RSS from 65,176 KiB in the demonstrated failing version to 172 KiB. Round-trips use system Git (`write-tree`, clean status, and strict fsck). Cancellation and configured pool thresholds have Rust guards.

Allocation profiling of `pick/one@synth-10k` drops from 312,599 to 287,497 calls (8.0%); reported peak heap is 7.34M in both. Instrumented wall time and RSS are excluded from the table.

Data: **`grit-utils/baselines/daily-checkout-chunks-before.json`**, **`grit-utils/baselines/daily-checkout-chunks-after.json`**. The detailed investigation, exact commands, samples, historical endpoint/direct-parent probes, GNU time output, syscall summaries, controls and validation limitations are in **`grit-utils/baselines/checkout-chunks-4567-audit.json`** and **`grit-utils/baselines/checkout-chunks-4567.md`**. The first bad commit was `63ea7f3ef`: eight-file cancellation flushes repeatedly failed the parallelism threshold. Its cancellation behavior remains supported.

### Results

<!-- grit:benchmark-tables -->

To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).

## Filtered revision counts (2026-10-26)

An unlimited `grit revs --count` with author, committer or message filters now counts during traversal. It reads each visited commit once into a reusable buffer, uses borrowed commit headers and keeps delta bases cached, rather than retaining per-commit maps and sorting a list. Excluded ancestry, shallow boundaries, grafts and existing filter semantics still apply.

Factory VM: AMD EPYC 9V45, 4 vCPUs, Rust 1.99.0, `release` profile, system Git 2.55.0. Baseline `dbdf37d29719` and changed `c17f3c85ffb8` ran in the same sitting. The daily harness supplies the 50,000-commit / 20,000-file deep-history fixture and the upstream Git v2.47.0 fixture. After one warmup, ten interleaved runs per tool alternate Git, baseline grit and changed grit, reversing tool order on every other run; builds and tests were idle. Wall time is the median measured with Python `perf_counter` around `/usr/bin/time -v`; RSS is the maximum process peak across those runs. Raw wall/RSS samples are in `grit-utils/baselines/filtered-revision-count.json`.

| Scenario | Git ms | Grit before ms | Grit after ms | Grit peak RSS before KiB | Grit peak RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `revs/count-author@deep-history` | 91.88 | 283.90 | 121.02 | 61,904 | 25,964 |
| `revs/author-n50@deep-history` | 2.17 | 3.63 | 3.64 | 17,332 | 17,408 |
| `log/author-n50@deep-history` | 2.10 | 3.56 | 3.58 | 17,724 | 17,792 |
| `revs/count-author@deep-history-cg` | 89.49 | 169.96 | 124.09 | 56,076 | 26,112 |
| `revs/author-n50@deep-history-cg` | 2.41 | 3.75 | 3.75 | 19,812 | 19,836 |
| `log/author-n50@deep-history-cg` | 2.35 | 3.64 | 3.65 | 19,876 | 20,096 |
| `revs/count-since@deep-history` | 2.09 | 2.89 | 2.92 | 15,312 | 15,276 |
| `revs/left-right-count@deep-history` | 2.75 | 5.40 | 5.46 | 15,784 | 15,740 |
| `revs/count@git-git` | 375.99 | 527.63 | 534.70 | 127,520 | 127,332 |
| `revs/count-author@git-git` | 479.37 | 917.16 | 385.81 | 129,796 | 59,256 |
| `revs/count-grep@git-git` | 486.45 | 885.70 | 366.93 | 123,720 | 59,244 |

The target drops from 3.09× to 1.32× Git time, with 57% less time and 58% less peak RSS. The other filtered counts improve by 27–59%; unchanged command paths differ by at most 1.4% in this run. Heaptrack records 1,247,228 → 266,825 allocations and 38.95 → 5.88 MB peak heap; callgrind records 4.38 → 2.14 billion instructions. `perf record -g` was blocked by the VM kernel setting, so callgrind and `strace -c` supplied CPU/syscall evidence.

`revs/count-path@git-git` could not run: system Git rejects the pinned `7e34d81` revision in the harness’s v2.47.0 fixture. The fixture and scenario definition were left unchanged.

### Commit-graph range counts follow-up

Filtered counts now use commit-graph parent links while marking excluded ancestry, and read object bodies for the included walk or for excluded commits missing from the graph. The reviewed implementation read all 49,500 excluded commit bodies for `HEAD~500..HEAD`; the new object-read regression test covers graph-enabled and disabled walks, merges and partial graph coverage. Shallow and grafted ranges are checked against system Git.

A second factory VM (Intel Xeon Platinum 8370C, 4 vCPUs, Rust 1.99.0, `release`, Git 2.55.0) compares origin/main `f0e7cddd530a`, reviewed branch `ca0542807699` and corrected code `0c235110d293` in the same sitting. The fixture and ten-run interleaved wall/RSS methodology match the earlier measurement; builds and tests were idle. Raw samples, command arguments and all three grit series are in `grit-utils/baselines/filtered-revision-count-range.json`. RSS below is the maximum process peak in KiB, from `/usr/bin/time -v`.

| Scenario | Git ms | Main ms | Reviewed ms | Fixed ms | Main RSS | Reviewed RSS | Fixed RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `log/author-n50@deep-history` | 2.59 | 5.05 | 5.25 | 4.77 | 15,968 | 16,100 | 17,036 |
| `revs/count-author@deep-history` | 151.22 | 533.64 | 207.67 | 204.06 | 60,872 | 24,968 | 25,880 |
| `revs/count-author-range@deep-history` | 4.24 | 325.87 | 201.85 | 196.57 | 54,132 | 25,240 | 26,068 |
| `revs/author-n50@deep-history` | 2.54 | 4.85 | 5.05 | 4.62 | 15,788 | 15,904 | 16,608 |
| `revs/count-grep-range@deep-history` | 4.13 | 326.81 | 201.35 | 195.83 | 53,912 | 25,040 | 25,800 |
| `revs/count@deep-history` | 112.74 | 6.96 | 6.61 | 6.06 | 15,200 | 15,232 | 16,424 |
| `log/author-n50@deep-history-cg` | 3.19 | 5.79 | 5.81 | 5.13 | 18,672 | 18,588 | 19,328 |
| `revs/count-author@deep-history-cg` | 159.65 | 312.30 | 208.71 | 205.47 | 55,072 | 25,168 | 26,004 |
| `revs/count-author-range@deep-history-cg` | 4.64 | 78.67 | 200.61 | 28.62 | 40,388 | 27,944 | 20,776 |
| `revs/author-n50@deep-history-cg` | 2.86 | 5.24 | 5.24 | 4.61 | 18,612 | 18,516 | 19,268 |
| `revs/count-grep-range@deep-history-cg` | 4.62 | 79.80 | 201.89 | 28.68 | 40,364 | 27,896 | 20,584 |
| `revs/count@deep-history-cg` | 23.45 | 6.77 | 6.89 | 6.75 | 15,444 | 15,428 | 16,372 |
| `revs/count-since@deep-history` | 2.28 | 3.82 | 3.88 | 3.42 | 13,692 | 13,996 | 14,988 |
| `revs/left-right-count@deep-history` | 3.74 | 8.62 | 8.71 | 8.07 | 14,132 | 14,464 | 15,408 |

The graph-backed author range count improves by 86% against the reviewed branch and 64% against main. Counts agree with Git, including 500 for the review reproduction. Full-history author counts retain the original optimization; graph loading is skipped when there are no exclusions. `perf record -g` remains unavailable (`perf_event_paranoid=4`); `strace -c` records about 1.4 ms in syscalls before the fix, while the deterministic graph-range test reduces object reads from 34 to 2.

This build records about 0.7–1.3 MiB more peak RSS on several unchanged paths than the reviewed build. A separate five-run `/proc/<pid>/smaps_rollup` sample of full-history author counting shows essentially unchanged peak anonymous memory (6,748 → 6,740 KiB), with the extra residency in file-backed mappings. Those samples are included in the baseline JSON; the RSS increases remain visible in the table rather than being treated as heap growth or omitted. No measured scenario slowed relative to main or the reviewed branch.

## Parallel worktree symlinks (2026-10-27)

Parallel worktree scans now compare physical symlinks through the same link-target reader and file-type classifier as serial scans. Regular files retain parallel hashing and streaming. Commit previews also use symlink metadata when loading a new link blob into their temporary object overlay.

Factory VM: AMD EPYC 7763, 4 vCPUs, Rust 1.99.0, `release` profile, stock Git 2.55.0. The baseline is origin/main `80dc322f7`; fixed results are from this change. Each fixture has 50,000 tracked regular files in 100 directories; the mixed fixture adds 64 directory symlinks. The edited case changes one regular file and, for the mixed fixture, retargets one link.

One warmup precedes seven interleaved runs per tool, reversing tool order on alternating runs. Wall time is the median measured with Python `perf_counter` around `/usr/bin/time -v`; RSS is the maximum process peak across runs. Builds and tests were idle. A saved Git-written index with unset stat data is restored before every command to force content verification; these are **not trusted-stat clean-tree timings**. Commands are `git diff --no-ext-diff` versus `grit --threads 4 diff --json`, and `git commit -a --dry-run -m bench` versus `grit --threads 4 commit --dry-run -m bench --json`.

| Fixture / operation | Git ms | Main ms | Fixed ms | Git RSS KiB | Main RSS KiB | Fixed RSS KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| regular diff, clean | 2623.28 | 198.22 | 196.33 | 26760 | 67188 | 67272 |
| regular preview, clean | 916.19 | 235.49 | 234.32 | 33420 | 73504 | 73804 |
| regular diff, edited | 2603.10 | 196.81 | 197.03 | 26768 | 67212 | 67216 |
| regular preview, edited | 913.63 | 366.93 | 365.37 | 33356 | 73816 | 73524 |
| mixed diff, clean | 2636.81 | 197.21 (error) | 198.56 | 26768 | 58680 | 67472 |
| mixed preview, clean | 918.62 | 214.50 (error) | 238.01 | 33424 | 64552 | 74624 |
| mixed diff, edited | 2612.90 | 196.00 (error) | 198.64 | 26772 | 58436 | 68336 |
| mixed preview, edited | 918.81 | 215.26 (error) | 372.02 | 33528 | 64988 | 74860 |

Regular-file timings stay within 1% of main and peak RSS within 0.5%. Mixed-fixture main results are directory-read errors; their time and RSS measure early failure and cannot be compared as successful scans. Fixed mixed diffs succeed; clean previews return `nothing_to_commit`, and edited previews succeed. Raw samples, exit codes, command arguments and fixture details are in `grit-utils/baselines/diff-parallel-symlinks-2076.json`.

The existing 50,000-file performance suite also passes all four checks with the fixed release CLI: warm clean diff is 148.49 ms versus Git 95.56 ms (1.55×), 100 edits are 150.88 versus 156.11 ms (0.97×), and mtime-only touches are 146.03 versus 94.83 ms (1.54×). The deletion parity check passes. These existing fixtures include deep paths and retain their original 3× timing limit.
