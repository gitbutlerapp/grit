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
- **Ratios** are **Grit mean ÷ Git mean**. Values below `1.00×` mean Grit is faster; values above `2.00×` are highlighted as regressions worth investigating.
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

### Results

<!-- grit:benchmark-tables -->

To refresh this page after updating a baseline JSON file, regenerate the static site:

```bash
make docs
```

The docs `--check` step fails if baseline numbers change without re-running `make docs`.

For other library surfaces under test, see the [grit-lib API on docs.rs](https://docs.rs/grit-lib).
