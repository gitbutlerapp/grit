# grit-utils

Utility crate for the Grit workspace. The primary binary is **`grit-bench`**, a scenario-based harness that compares **`grit`** and **`git`** using [hyperfine](https://github.com/sharkdp/hyperfine).

## Requirements

- **`hyperfine`** on `PATH` (install via your package manager or `cargo install hyperfine`). If hyperfine is missing, `grit-bench` exits with an explicit error.
- Built **`grit`** binary (defaults to `../target/release/grit` from this crate, then `PATH`).
- System **`git`**.

## Usage

```bash
cargo build --release -p grit-cli -p grit-utils

# Status scenarios (dirty + clean) at selected file counts
./target/release/grit-bench status --sizes 100,1000 --format text

# Add scenario
./target/release/grit-bench add --sizes 1000

# Commit scenario (grit commit vs git add -A && git commit), config-isolated
./target/release/grit-bench commit --sizes 10000,100000 --format json --output baselines/commit-after-LH.json

# Hot-path scenarios (switch / pick / merge / pick-series) at L and H
./target/release/grit-bench hot-paths --sizes 10000,100000 --format json --output baselines/hot-paths-before.json

# ODB backend scenarios (cat-file batch/check, rev-list --objects) on repacked 100k repo
./target/release/grit-bench odb-backend --format json --output baselines/odb-backend-before.json

# Optional FSMN index extension (core.fsmonitor hook v2)
./target/release/grit-bench hot-paths --sizes 10000 --fsmonitor-fixture

# All status/add scenarios
./target/release/grit-bench all --sizes 100,1000,10000

# Compare two JSON reports (exit 1 if any scenario ratio drifts > tolerance)
./target/release/grit-bench compare baseline.json candidate.json --tolerance 0.10
```

Global options:

| Flag | Default | Description |
| --- | --- | --- |
| `--format` | `text` | `text`, `markdown`, or `json` |
| `--output` | stdout | Write report to a file |
| `--warmup` | `3` | hyperfine warmup runs |
| `--min-runs` | `5` | hyperfine minimum timed runs |
| `--timestamp` | now (UTC) | RFC3339 timestamp stored in JSON |
| `--grit` / `--git` | auto | Explicit binary paths |

## JSON report schema (v1)

Reports use `"schema_version": 1`. All times in scenario stats are **milliseconds** (converted from hyperfine’s seconds).

```json
{
  "schema_version": 1,
  "timestamp": "2026-10-07T12:00:00Z",
  "machine": {
    "cpu_model": "…",
    "physical_cores": 8,
    "logical_cores": 16,
    "ram_bytes": 67108864000,
    "os": "linux",
    "kernel": "Linux 6.12 …",
    "scratch_filesystem": "ext4",
    "rustc_version": "rustc 1.85 …",
    "cargo_profile": "release"
  },
  "tools": {
    "git": "git version 2.43.0",
    "grit": "grit 0.5.0",
    "grit_commit": "abc123…"
  },
  "scenarios": [
    {
      "id": "status-dirty-1000",
      "group": "status",
      "fixture": "synthetic-1000",
      "description": "…",
      "driver": "cli",
      "git": {
        "mean_ms": 12.3,
        "median_ms": 12.0,
        "stddev_ms": 0.5,
        "min_ms": 11.0,
        "max_ms": 13.0,
        "runs_ms": [11.0, 12.0, 13.0]
      },
      "grit": { "…": "…" },
      "ratio": 0.85
    }
  ]
}
```

**Ratio** is `grit_median_ms / git_median_ms`. Values below `1.0` mean grit is faster on median time.

### Scenario groups (current)

| ID pattern | Group | What it measures |
| --- | --- | --- |
| `status-dirty-{N}` | status | Dirty tree (~10% modified, ~5% untracked): `git status -s` vs `grit status` |
| `status-clean-{N}` | status | Clean tree: `git status -s` vs `grit status` |
| `add-{N}` | add | `git add -A` vs `grit add` (stage all); **git** resets the index between timed runs |
| `switch-{N}` | switch | Two branches differing in ~50 paths; `git switch -q X && switch -q Y` vs `grit switch X && switch Y` |
| `switch-wide-{N}` | switch | ~10% path delta including directory deletions; same switch pattern |
| `pick-{N}` | pick | One commit touching ~2000 paths (scaled down for small `N`); `git cherry-pick` vs `grit pick`; reset between runs |
| `merge-{N}` | merge | Same topology as pick; `git merge -q --no-edit` vs `grit merge` |
| `pick-series-{N}` | pick | 20 commits: `git cherry-pick base..topic` vs 20 sequential `grit pick` (upper bound — process startup) |

Hot-path scenarios set `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL` to an empty file. Append `-fsmn` to the scenario id when `--fsmonitor-fixture` is used (trivial hook v2 + FSMN index extension).

**L / H fixtures:** `N=10000` with 100 directories and 1000 commits of history; `N=100000` with 1000 directories. Criterion micro-benchmarks for the same shapes live in `grit-lib/benches/hot_paths.rs`.

Fixtures are synthetic repos with `N` tracked text files under `/tmp/grit-bench-scratch`.

## Development

```bash
cargo test -p grit-utils
cargo clippy -p grit-utils -- -D warnings
```

Unit tests cover hyperfine JSON parsing (checked-in sample), median ratio math, compare tolerance, and markdown rendering.
