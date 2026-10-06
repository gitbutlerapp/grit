# Benchmark machine record

- **Date (UTC):** 2026-10-06
- **CPU:** Intel(R) Xeon(R) Processor (4 vCPUs, 1 thread/core)
- **RAM:** 16 GiB
- **OS / kernel:** Linux 6.12.94+ x86_64 (Cursor cloud VM)
- **Git:** `git version 2.43.0` (`/usr/bin/git`)
- **Grit commit:** `bb7404251fc6da01804a5fb7a682cddf7084b3a3` (repo HEAD at measurement time)
- **Rust:** `rustc 1.99.0 (b940084d7 2026-09-28)`
- **Hyperfine:** 1.18.0 (Ubuntu package)

## Commands

```bash
rustup default stable
cargo build --release -p grit-git -p grit-cli
bash bench/run-everyday.sh --scales S,M,L
bash bench/run.sh
python3 bench/report.py
```

## Notes

- **H scale skipped** intentionally (deep-history sweep; not re-run on this VM).
- Everyday suite uses `BENCH_WARMUP=2`, `BENCH_MIN_RUNS=10` (defaults in `bench/run-everyday.sh`).
- Plumbing suite uses `WARMUP=2`, `MIN_RUNS=10` in `bench/run.sh`.
- Bench scratch: `/tmp/grit-bench-everyday`, `/tmp/grit-bench-scratch`.
- Cloud agent git was configured with `commit.gpgsign=false` in `bench/run.sh` to avoid signing hook failures.
