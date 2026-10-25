# grit-utils

Utility crate for the Grit workspace. The primary binary is **`grit-bench`**, a scenario-based harness that compares **`grit`** and **`git`** using [hyperfine](https://github.com/sharkdp/hyperfine).

Daily performance census reports use the [`grit-daily-bench/1`](DAILY-SCHEMA.md) JSON schema.

```bash
# Headline smoke (13 local scenarios; direct-spawn timing + peak RSS)
./target/release/grit-bench fixtures prepare --only synth-10k-smoke,many-refs-smoke,git-git-wt
./target/release/grit-bench daily --smoke --offline --output target/bench/daily-smoke.json
./target/release/grit-bench daily validate target/bench/daily-smoke.json

# Filters and baseline grit on headline rows
./target/release/grit-bench daily --only status/clean@synth-10k --area worktree --smoke
./target/release/grit-bench daily --smoke --baseline-grit /path/to/older/grit --output /tmp/daily.json
```

## Requirements

- **`hyperfine`** on `PATH` (install via your package manager or `cargo install hyperfine`). If hyperfine is missing, `grit-bench` exits with an explicit error.
- Built **`grit`** binary (defaults to `../target/dist/grit`, then `../target/release/grit`, then `PATH`).
- System **`git`**.

## Usage

```bash
cargo build --release -p grit-cli -p grit-utils

# Fixture cache (git.git at pinned tag + synthetic tiers S/M/L)
./target/release/grit-bench fixtures list --format json
./target/release/grit-bench fixtures prepare --format markdown
./target/release/grit-bench fixtures prepare --offline --only git-git   # fail if git.git missing

# Deterministic synthetic benchmark repo (grit-lib backed, seeded)
./target/release/grit-bench fixture synth --tier S --out /tmp/synth-s
./target/release/grit-bench fixture synth --tier M --out /tmp/synth-m --format json

# Status scenarios (dirty + clean) at selected file counts
./target/release/grit-bench status --sizes 100,1000 --format text

# Add scenario
./target/release/grit-bench add --sizes 1000

# Initial `add .` on untracked files in many small directories (flat + d<i%200>/s<i%37> layouts)
./target/release/grit-bench add-many-dirs --dirs 4000 --nested-files 49000 --format json --output baselines/add-many-dirs.json

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

# History scenarios (log, show, rev-list count, blame, cat-file batch) on synthetic fixtures
./target/release/grit-bench history --format json --output baselines/history-synth-L.json
./target/release/grit-bench history --smoke --format json --output /tmp/history-smoke.json

# History rewrite (reword, drop, fixup, squash) vs git history / git rebase
./target/release/grit-bench rewrite-history --format json --output baselines/history.json
./target/release/grit-bench rewrite-history --smoke --format json --output /tmp/history-rewrite-smoke.json

# Worktree scenarios on a cached registry fixture (status / add / commit / diff)
./target/release/grit-bench fixtures prepare --only synth-s
./target/release/grit-bench run --group worktree --fixture synth-s --format json --output /tmp/worktree-synth-s.json
# Compare two JSON reports (exit 1 if any scenario ratio drifts > tolerance)
./target/release/grit-bench compare baseline.json candidate.json --tolerance 0.10

# Campaign scorecard on real fixtures (git.git + chromium-class synthetic; optional linux)
./target/release/grit-bench campaign --format json --output baselines/campaign-before.json

# 1.1× ledger table: writes markdown to `--output` (JSON is omitted for that combination)
./target/release/grit-bench campaign --ledger --format markdown --output baselines/campaign-before.ledger.md

# Ledger plus JSON: write ledger next to the JSON path
./target/release/grit-bench campaign --ledger --format json --output baselines/campaign-before.json

# Classify a committed report without re-running hyperfine (exits 1 if any `fail` row)
./target/release/grit-bench campaign --ledger --from baselines/campaign-after.json

# Smaller fixtures for CI (`GRIT_BENCH_LINUX=1` or `--with-linux` adds linux)
./target/release/grit-bench campaign --smoke --no-network --format json --output /tmp/campaign-smoke.json

# Production baseline (two consecutive runs, ±10% compare) — see scripts/capture-production-campaign-baseline.sh
./grit-utils/scripts/capture-production-campaign-baseline.sh
```

The committed **`campaign-before.json`** / **`campaign-after.json`** pair is captured with **`--warmup 0 --min-runs 5 --no-network`** on both runs (full production fixture matrix). The capture script retries the consecutive pair until **`grit-bench compare`** is within ±10% (override attempts with **`GRIT_BENCH_CAMPAIGN_COMPARE_ATTEMPTS`**, default 8). Re-run the script above to refresh; it writes the compare transcript to **`campaign-consecutive-compare.txt`**.

Committed production scorecard artifacts:

| File | Role |
| --- | --- |
| `baselines/campaign-before.json` | First production campaign capture |
| `baselines/campaign-after.json` | Second consecutive capture (same VM, same flags) |
| `baselines/campaign-consecutive-compare.txt` | `grit-bench compare … --tolerance 0.10` output (exit 0 when stable) |
| `baselines/campaign-before.ledger.md` | 1.1× ledger from the first capture |

### Campaign fixtures

| Fixture | Source | Enable |
| --- | --- | --- |
| **git-dot-git** | Upstream [git.git](https://github.com/git/git) at pinned tag (`GRIT_BENCH_GIT_GIT_TAG`, default `v2.47.0`), bare + worktree, repacked with pack bitmaps | always |
| **chromium-synth** | Deterministic `git fast-import` repo (400k files, 40k dirs, 100k commits, 50k refs, single pack + commit-graph) cached under the bench scratch root | always |
| **linux** | [torvalds/linux](https://github.com/torvalds/linux) at pinned tag (`GRIT_BENCH_LINUX_TAG`, default `v6.12`) | `--with-linux` or `GRIT_BENCH_LINUX=1` |

Performance targets and documented gaps live in [`perf-targets.toml`](perf-targets.toml). `campaign --ledger` classifies each scenario as `pass`, `fail`, `documented`, or `owned-by-item-14` (rev-list count/objects and serve-clone rows are not scored in this step).

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
  "fixtures": [
    {
      "name": "synth-s",
      "tier": "S",
      "head_oid": "abc123…",
      "object_count": 12345,
      "ref_count": 1000,
      "file_count": 2000
    }
  ],
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

### Fixture cache

Prepared fixtures live under **`GRIT_BENCH_CACHE`** (default `$XDG_CACHE_HOME/grit-bench` or `~/.cache/grit-bench`):

| Name | Contents |
| --- | --- |
| `git.git` | Bare upstream [git.git](https://github.com/git/git) at tag **`v2.47.0`** (`GRIT_BENCH_GIT_GIT_TAG`); `HEAD` must match the pinned oid |
| `synth-s`, `synth-m`, `synth-l` | Deterministic grit-lib synthetic tiers (same shapes as `fixture synth`) |

Mutating scenarios should take a **`git clone --local`** copy via [`fixture_registry::copy_for_use`](src/fixture_registry.rs) so the cache stays immutable.

### Scenario groups (current)

| ID pattern | Group | What it measures |
| --- | --- | --- |
| `status-dirty-{N}` | status | Dirty tree (~10% modified, ~5% untracked): `git status -s` vs `grit status` |
| `status-clean-{N}` | status | Clean tree: `git status -s` vs `grit status` |
| `status-after-checkout-{N}` | status | First status after a checkout: every entry racily clean (files and index in one timestamp second); each run restores that index |
| `add-{N}` | add | `git add -A` vs `grit add` (stage all); **git** resets the index between timed runs |
| `switch-{N}` | switch | Two branches differing in ~50 paths; `git switch -q X && switch -q Y` vs `grit switch X && switch Y` |
| `switch-wide-{N}` | switch | ~10% path delta including directory deletions; same switch pattern |
| `pick-{N}` | pick | One commit touching ~2000 paths (scaled down for small `N`); `git cherry-pick` vs `grit pick`; reset between runs |
| `revert-{N}` | pick | Same fixture as pick; `git revert --no-edit` vs `grit revert` on the topic tip; reset between runs |
| `merge-{N}` | merge | Same topology as pick; `git merge -q --no-edit` vs `grit merge` |
| `pick-series-{N}` | pick | 20 commits: `git cherry-pick base..topic` vs 20 sequential `grit pick` (upper bound — process startup) |
| `history-log-first-{N}` | history | First *N* commits (`git log -N --oneline` vs paged `grit log`); **`driver: cli`** |
| `history-log-all` | history | Full `main` history on the synthetic fixture; **`driver: cli`** |
| `history-rev-list-count` | history | `git rev-list --count HEAD` vs `grit-bench lib-op rev-list-count-head`; **`driver: lib`** |
| `history-blame` | history | High-churn path: `git blame --porcelain` vs `grit-bench lib-op blame-porcelain`; **`driver: lib`** |
| `history-blame-large-full` | history | Full-file blame on 5000-line tail-churn fixture vs `git blame --porcelain`; **`driver: cli`** (non-smoke) |
| `history-blame-large-range` | history | `blame -L 100,120` on the same fixture vs git; **`driver: cli`** (non-smoke) |
| `history-show` | history | One commit (`git show` vs `grit show`); **`driver: cli`** |
| `history-cat-file-batch` | history | Sorted OIDs reachable from `HEAD` fed to `cat-file --batch`; grit side uses **`lib-op cat-file-batch`**; **`driver: lib`** |

Library-backed scenarios (`driver: lib`) run through the hidden `grit-bench lib-op` subcommand (same workloads as `grit-bench drive` for cat-file batch). Correctness is checked before timing: rev-list count equality, blame porcelain line attribution, and byte-identical cat-file batch output vs system `git`.

**Worktree group** (`grit-bench run --group worktree --fixture <kind>`) runs on registry fixtures (`synth-s`, `synth-m`, `synth-l`, `git-git`). Each scenario uses hyperfine `--prepare` to reset the clone identically for git and grit (always back to a pinned fixture-base commit, especially before commit iterations); stdout/stderr are discarded (`> /dev/null`). `--smoke` uses hyperfine `--runs 1` (exactly one timed sample per tool). Repos pin `core.untrackedCache=false` and `core.fsmonitor=false` locally, with the same isolated git env as other suites.

| ID pattern | What it measures |
| --- | --- |
| `worktree-status-clean-{fixture}` | Clean tree: `git status -s` vs `grit status` |
| `worktree-status-dirty-{fixture}` | ~1% tracked files modified plus ~1% untracked files |
| `worktree-add-{fixture}` | Stage the modified path set (`git add` paths vs `grit add`) |
| `worktree-commit-{fixture}` | Commit after staging ~1% modified paths |
| `worktree-diff-{fixture}` | Unstaged worktree vs index (`git diff` vs `grit diff`) |

Hot-path scenarios set `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL` to an empty file. Append `-fsmn` to the scenario id when `--fsmonitor-fixture` is used (trivial hook v2 + FSMN index extension).

**L / H fixtures:** `N=10000` with 100 directories and 1000 commits of history; `N=100000` with 1000 directories. Criterion micro-benchmarks for the same shapes live in `grit-lib/benches/hot_paths.rs`.

Fixtures are synthetic repos with `N` tracked text files under `/tmp/grit-bench-scratch`.

### Synthetic fixture tiers (`fixture synth`)

| Tier | Files | Commits | Refs | Notes |
| --- | ---: | ---: | ---: | --- |
| `S` | 2 000 | 500 | 1 000 | CI integration test |
| `M` | 20 000 | 5 000 | 10 000 | medium benchmarks |
| `L` | 100 000 | 50 000 | 100 000 | deep history, ~1 GiB+ uncompressed pack, loose objects + checked-out worktree |

The same tier and seed always produce the same `HEAD` OID. Generation time is printed to stderr; `--format json` adds structured fields including `generation_ms` and `pack_uncompressed_bytes`.

## Development

```bash
cargo test -p grit-utils
cargo clippy -p grit-utils -- -D warnings
```

Unit tests cover hyperfine JSON parsing (checked-in sample), median ratio math, compare tolerance, and markdown rendering.
