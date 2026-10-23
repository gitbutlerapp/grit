---
title: Performance overview (daily matrix)
summary: Factory daily census — grit vs git time and peak memory across functional areas.
---

The factory runs a **daily census** of common `grit` workflows against the **system** `git` binary on fixed repository shapes. Results land in committed JSON (`grit-daily-bench/1`) and on the [cooking bench dashboard](https://maint.grit-scm.com/cooking/bench). This page renders the latest committed baseline as a per-area matrix aligned with [`grit-utils/MATRIX.md`](https://github.com/gitbutlerapp/grit/blob/main/grit-utils/MATRIX.md).

## Methodology

### Repository shapes

Scenarios use prepared fixtures under **`GRIT_BENCH_CACHE`** (see [Benchmarks](benchmarks.md)). Shapes include:

| Shape | Role |
| --- | --- |
| `small` | Tiny synthetic repo for cheap init/commit checks |
| `synth-10k` / `synth-100k` | Large synthetic trees (10k / 100k files) with controlled dirty/clean states |
| `git-git` | Bare and working copies of upstream **git.git** (tag pinned in the fixture registry) |
| `many-refs` / `many-refs-loose` | Many branches/tags for ref-list and pack-refs workloads |
| `big-blobs` | Few paths with very large blobs (sparse worktree for daily capture on low-memory VMs) |
| `deep-tree` | Deep directory nesting for status/diff on deep paths |

Full captures may set **`GRIT_BENCH_DAILY_LOW_MEMORY=1`** so special shapes use smoke-sized variants while keeping the same scenario ids (documented in [Benchmarking harness](benchmarking.md)).

### Run policy

`grit-bench daily` uses the **direct-spawn runner**: each timed sample spawns `git` or `grit` as a child process, measures **wall time** for the process lifetime, and records **peak resident set size (RSS)** for that child. On Linux, peak RSS comes from **`wait4(2)`** (`ru_maxrss`, converted to bytes). Each tool reports the **median** wall time and **median** peak RSS over successful timed runs (default policy: warmups plus at least five timed samples and a minimum aggregated timed window per scenario).

Headline scenarios (`headline: true` in the report) are the **17 cooking-page rows**; they are highlighted in the tables below.

### Invocation

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench fixtures prepare   # or the subset from capture-daily-baseline.sh
./target/release/grit-bench daily --output grit-utils/baselines/daily/latest.json
./target/release/grit-bench daily validate grit-utils/baselines/daily/latest.json
```

Factory VMs merge per-scenario captures when memory is tight; regenerate **`latest.json`** / **`second.json`** with **`grit-utils/scripts/capture-daily-baseline.sh`** and verify stability via **`grit-bench daily compare`** (see [Benchmarking harness](benchmarking.md)).

Rows marked **Pending** or **Not in committed daily baseline** are exemptions: either blocked on roadmap CLI (see **Gaps** in MATRIX.md) or not yet present in the committed census file.

## Daily matrix (committed baseline)

<!-- grit:daily-matrix -->
