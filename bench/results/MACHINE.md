# Benchmark machine record

- **Date (UTC):** 2026-10-06
- **CPU:** Intel(R) Xeon(R) Processor (4 vCPUs, 1 thread/core)
- **RAM:** 16 GiB
- **OS / kernel:** Linux 6.12.94+ x86_64 (Cursor cloud VM)
- **Git:** `git version 2.43.0` (`/usr/bin/git`)
- **Grit source commit:** `7967f0c3b1ba7052f5840c56d3099f560f75f1d7` (reviewed branch tip; used to build `target/release/grit-git` via `cargo build --release -p grit-git`)
- **Rust:** `rustc 1.99.0 (b940084d7 2026-09-28)`
- **Hyperfine:** 1.18.0 (Ubuntu package)

## Commands

```bash
rustup default stable
git checkout 7967f0c3b1ba7052f5840c56d3099f560f75f1d7  # grit source tree for the measured binary
cargo build --release -p grit-git -p grit-cli
bash bench/run-everyday.sh --scales S,M,L,H
bash bench/run.sh
bash bench/run.sh branch for-each-ref config show-ref read-tree checkout checkout-index reset rm mv check-ignore symbolic-ref update-ref merge-base commit-tree name-rev show stripspace count-objects tag tag-list
python3 bench/report.py
```

## Notes

- **H scale:** Full everyday **H** sweep completed (~5 minutes wall clock for all 24 hyperfine exports). No single scenario reached the ~10 minute skip threshold; slowest **mean** at H was **rebase@H** (~5.4 s grit mean per hyperfine).
- Everyday suite uses `BENCH_WARMUP=2`, `BENCH_MIN_RUNS=10` (defaults in `bench/run-everyday.sh`).
- Plumbing suite uses `WARMUP=2`, `MIN_RUNS=10` in `bench/run.sh`.
- Bench scratch: `/tmp/grit-bench-everyday`, `/tmp/grit-bench-scratch`.
- Cloud agent git was configured with `commit.gpgsign=false` in `bench/run.sh` to avoid signing hook failures.
