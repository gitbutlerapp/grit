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

# All ported scenarios
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
| `add-{N}` | add | `add -A` after modifying ~20% of files (index reset each hyperfine run) |

Fixtures are synthetic repos with `N` tracked text files under `/tmp/grit-bench-scratch`.

## Development

```bash
cargo test -p grit-utils
cargo clippy -p grit-utils -- -D warnings
```

Unit tests cover hyperfine JSON parsing (checked-in sample), median ratio math, compare tolerance, and markdown rendering.
