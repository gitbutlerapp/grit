---
title: Benchmarks
summary: How grit performance compares to Git.
---

Performance work is measured on the factory VM and in CI against the **system** `git` binary. Micro-benchmarks live in `grit-lib` (Criterion) and command-level scenarios in `grit-bench` (`grit-utils`).

## grit-lib: parallel object hashing

Criterion group `hash/batch_parallel` (`cargo bench -p grit-lib --bench objects hash/batch_parallel`) hashes **20 000** Git blob objects of **16 KiB** each using [`hash_objects_parallel`](https://docs.rs/grit-lib/latest/grit_lib/hash/fn.hash_objects_parallel.html) (SHA-1, release build).

| Threads | Time (median) | Throughput |
| ------- | ------------- | ---------- |
| 1       | 173 ms        | 1.76 GiB/s |
| 8       | 44.5 ms       | 6.86 GiB/s |

**Speedup (8 vs 1 thread): ~3.9×** on the factory VM (2026-10-07).

Parallel hashing falls back to a serial loop when there are fewer than [`PAR_HASH_MIN_ITEMS`](https://docs.rs/grit-lib/latest/grit_lib/hash/constant.PAR_HASH_MIN_ITEMS.html) (32) objects or less than [`PAR_HASH_MIN_TOTAL_BYTES`](https://docs.rs/grit-lib/latest/grit_lib/hash/constant.PAR_HASH_MIN_TOTAL_BYTES.html) (256 KiB) of payload, so small batches avoid thread overhead.

For broader command comparisons and baselines, see the generated benchmarks page when present in the docs tree and the [grit-lib API on docs.rs](https://docs.rs/grit-lib) for the surfaces under test.
