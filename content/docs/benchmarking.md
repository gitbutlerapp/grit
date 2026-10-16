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
