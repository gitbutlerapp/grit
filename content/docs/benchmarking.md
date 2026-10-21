# Benchmark harness (`make bench-lib` / `make bench-scenarios`)

Grit compares library micro-benchmarks (Criterion) and command-level scenarios (`grit-bench` + hyperfine) against the system **`git`** binary on the same machine. Two Make targets run the full suites from a clean checkout after building release binaries.

## `make bench-lib`

Runs every Criterion bench under `grit-lib/benches/` (including `unstable-bench-helpers` where required), then writes a JSON summary:

| Output | Description |
| --- | --- |
| `target/criterion/` | Raw Criterion results (`estimates.json` per benchmark) |
| `target/bench/criterion-summary.json` | Aggregated summary produced by `grit-bench criterion-summary` |

```bash
make bench-lib
```

### Criterion summary JSON (`criterion-summary.json`)

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | `1` | Summary schema version |
| `timestamp` | RFC3339 string | Capture time (UTC) |
| `machine` | object | CPU, RAM, kernel, `rustc`, cargo profile (same shape as grit-bench v1) |
| `tools` | object | `git` is `n/a (criterion micro-benchmarks)`; `grit` identifies the library build |
| `scenarios[]` | array | One row per Criterion `new/estimates.json` |
| `scenarios[].driver` | `"criterion"` | Not a git vs grit hyperfine row |
| `scenarios[].grit` | timing block | Mean/median/stddev in **milliseconds** (converted from Criterion nanoseconds) |

Each scenario `id` is derived from the path under `target/criterion/` (for example `criterion-hash_sha1-4KiB`).

## `make bench-scenarios`

Prepares registry fixtures, runs **every grit-bench scenario group** for the chosen synthetic tier, and writes both JSON and a markdown table:

| Output | Description |
| --- | --- |
| `target/bench/scenarios-<host>.json` | Merged grit-bench report (`schema_version: 1`) |
| `target/bench/scenarios-<host>.md` | Human-readable ratio table |

```bash
# Default tier M (full hyperfine samples)
make bench-scenarios TIER=M

# Small fixtures + one timed iteration per scenario (CI smoke)
make bench-scenarios TIER=S SMOKE=1
```

### Tier (`TIER=S|M|L`)

| Tier | Registry fixture | Typical use |
| --- | --- | --- |
| **S** | `synth-s` | CI smoke, fast local checks |
| **M** | `synth-m` | Default local run |
| **L** | `synth-l` | Large synthetic repo (long run) |

`SMOKE=1` sets `--smoke` on `grit-bench scenarios`: warmup `0`, exactly **one** hyperfine run per scenario, tiny network/parallel fixtures, and skips heavy groups (ODB backend, bitmaps, refs). CI uses `TIER=S SMOKE=1`; **timings are not gated**—the job fails only if a scenario errors.

### Scenario report JSON (`scenarios-<host>.json`)

Uses the same **grit-bench v1** schema as individual subcommands (`schema_version`, `machine`, `tools`, `fixtures`, `scenarios`). Each scenario includes `git` and `grit` timing blocks (milliseconds) and `ratio` (`grit_median_ms / git_median_ms`).

Groups included in the merged report:

| Group | Source |
| --- | --- |
| `worktree` | status / add / commit / diff on the tier fixture |
| `history` | log, show, rev-list, blame, cat-file batch |
| `rewrite-history` | `grit history` reword/drop/fixup/squash vs git history / git rebase |
| `network` | clone, fetch, push, ls-remote (file + smart HTTP) |
| `parallel` | threads=1 vs auto on the 100k-class fixture |
| `revwalk`, `fsck`, `maintenance` | porcelain vs git |
| `resolve-cat`, `ignore`, `local-clone`, `hot-paths` | CLI hot paths |
| `deep-leaf`, `dense-leaf` | smoke layout only when `SMOKE=1` |
| `odb-backend`, `bitmaps`, `refs` | full runs only (`SMOKE=0`) |

Equivalent CLI:

```bash
cargo build --release -p grit-cli -p grit-http-server -p grit-utils
./target/release/grit-bench scenarios --tier S --smoke --format json --output target/bench/scenarios-smoke.json
./target/release/grit-bench criterion-summary --criterion-dir target/criterion --format json --output target/bench/criterion-summary.json
```

See also [Benchmarks](benchmarks.md) for committed baselines and acceptance ratios.

## `grit-bench daily` (factory census)

Headline scenarios compare **`grit`** and system **`git`** with the direct-spawn runner (wall time + peak RSS per run). Reports use the [`grit-daily-bench/1`](../grit-utils/DAILY-SCHEMA.md) schema (distinct from hyperfine v1).

```bash
cargo build --release -p grit-cli -p grit-utils
./target/release/grit-bench fixtures prepare --only synth-10k-smoke,many-refs-smoke,git-git-wt,git-git
./target/release/grit-bench daily --smoke --offline --output target/bench/daily-smoke.json
./target/release/grit-bench daily validate target/bench/daily-smoke.json
```

| Flag | Meaning |
| --- | --- |
| `--smoke` | Smoke fixtures, **0** warmups, **1** timed run per tool |
| `--only id,...` | Run a subset of scenario ids |
| `--area <area>` | Filter by functional area (`worktree`, `branching`, `remotes`, …) |
| `--baseline-grit <path>` | Also time a second grit binary on headline rows |
| `--offline` | Do not clone `git.git` (fixtures must already be cached) |

All **17 headline** ids run in **`--smoke`** mode (including the four **remote** headlines on cached `git-git` bare and synth-10k bare remotes). Additional **remote matrix** rows (`fetch/noop`, `ls-remote/all`, HTTP clone/fetch/push) run on full daily reports and in `grit-utils` remote e2e tests. CI job **`bench-daily-smoke`** runs the smoke command above; timings are not gated.

### Consecutive-run stability (`daily compare`)

Full factory captures run **`grit-bench daily`** twice back-to-back (release build, fixtures prepared, optional **`--baseline-grit`** pointed at the latest release binary). Stability is checked with:

```bash
./target/release/grit-bench daily compare grit-utils/baselines/daily/latest.json \
  grit-utils/baselines/daily/second.json --tolerance 0.10 --markdown
```

Time-ratio drift applies when git median wall time is **above 20 ms** on both captures; peak-RSS ratios are always compared when medians are stable. The committed transcript lives in **`grit-utils/baselines/daily/consecutive-compare.txt`** (exit **0** at ±10% tolerance).

Regenerate the pair with **`grit-utils/scripts/capture-daily-baseline.sh`** after building release binaries and setting **`GRIT_BENCH_BASELINE_GRIT`**. The capture script prepares only fixtures the daily matrix uses (not full **`synth-l`**), sets **`GRIT_BENCH_DAILY_LOW_MEMORY=1`** (smoke-sized **big-blobs** / **deep-tree** / **many-refs** fixtures while keeping `@big-blobs` scenario ids), runs **`daily --only <id>`** once per executable scenario (`daily list-ids`), and merges partial JSON reports so peak RSS stays within factory VM limits.

| Baseline file | Role |
| --- | --- |
| `grit-utils/baselines/daily/latest.json` | First consecutive full daily report |
| `grit-utils/baselines/daily/second.json` | Second consecutive full daily report |
| `grit-utils/baselines/daily/consecutive-compare.txt` | `daily compare … --tolerance 0.10` output |

**Factory VM (4 logical CPUs, 2026-10-21):** fixture prepare for the daily subset took **968 s** wall. A committed consecutive pair (**`latest.json`** + **`second.json`**, 58 registry rows with **57** timed + **1** pending) took **1476 s** and **1478 s** wall per merged run (**2954 s** total, under the **4 h** wall budget on this 4-core host; **8-core equivalent 2 h** budget). Compare at **`0.10`** tolerance exited **0** (`consecutive-compare.txt`).

Default daily **`RunPolicy`** was tightened for baseline stability: **2** warmups per tool, **5** minimum timed runs, **5 s** minimum aggregated timed window, **180 s** scenario budget.
