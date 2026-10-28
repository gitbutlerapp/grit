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

### Patch parsing and staging

`apply_blob` includes `parse_patch_small`, which parses a valid traditional unified diff and fails if parsing fails. It measures parsing independently of repository discovery, object writes, and index updates:

```bash
cargo bench --profile dist -p grit-lib --bench apply_blob -- parse_patch_small
```

For a command-level comparison with system Git, `add/patch-file@synth-10k` applies a worktree patch to the index with `grit add --patch-file` and `git apply --cached`:

```bash
cargo build --profile dist -p grit-cli -p grit-utils
./target/dist/grit-bench daily --only add/patch-file@synth-10k --output target/bench/patch-file.json
```

Use the same fixture, build profile, and system Git for before/after comparisons. Each timed mutation must start with the same index and worktree state; preparation runs outside the timed region.

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

`grit-bench partial-clone [--files N] [--packs N]` runs on its own, outside the merged report: a `blob:none` `file://` clone of a source holding `--packs` packs (50 by default, one incremental repack per batch of files), and the `reset --hard` in a blobless clone that then lazily fetches every `HEAD` blob, both against Git.

Equivalent CLI:

```bash
cargo build --release -p grit-cli -p grit-http-server -p grit-utils
./target/release/grit-bench scenarios --tier S --smoke --format json --output target/bench/scenarios-smoke.json
./target/release/grit-bench criterion-summary --criterion-dir target/criterion --format json --output target/bench/criterion-summary.json
```

See also [Benchmarks](benchmarks.md) for committed baselines and acceptance ratios.

## `grit-bench daily` (factory census)

Use `--min-runs 10` for performance investigations that require at least ten
samples per tool. Daily runs honor this common option, raising the usual run cap
when the requested minimum is larger. `--smoke` still uses one sample, and the
scenario time budget can stop expensive scenarios early; inspect each row's `runs`.
Daily reports reject a plan whose two commands are placeholders instead of an
operation. Reword uses a scripted editor with `git history` on supporting Git
versions; deepen starts from one branch boundary with tags excluded and checks
that Git and grit produce identical new shallow boundaries.


Headline scenarios compare **`grit`** and system **`git`** with the direct-spawn runner (wall time + peak RSS per run). Reports use the [`grit-daily-bench/1`](../grit-utils/DAILY-SCHEMA.md) schema (distinct from hyperfine v1).

Published daily numbers (the [factory dashboard](https://maint.grit-scm.com/cooking/bench) and the [Performance overview](performance-overview.md)) measure the **`dist`** profile, the fat-LTO build that releases ship. Build `grit-bench` in the same profile: it then times `target/dist/grit` by default and records `"cargo_profile": "dist"` in the report.

```bash
cargo build --profile dist -p grit-cli -p grit-utils
./target/dist/grit-bench fixtures prepare --only synth-10k-smoke,many-refs-smoke,git-git-wt,git-git
./target/dist/grit-bench daily --smoke --offline --output target/bench/daily-smoke.json
./target/dist/grit-bench daily validate target/bench/daily-smoke.json
```

The daily census switched from `release` to `dist` on 2026-10-28. That one-time switch moves the baseline of **every** scenario: short commands drop by roughly 10–15% in time and 20% in peak RSS (`show/head@git-git` went from 3.08 to 2.70 ms and from 16.1 to 13.1 MB on the same VM), because LTO packs the code a run touches into fewer pages. Read the step at that date as a build change, not a run of wins, and compare reports only within one profile (check `machine.cargo_profile`).

A `release` build stays fine for iterating on a change (it rebuilds about three times faster), as long as the before and after binaries use the same profile. Code-layout noise is larger in `release`: any change to `grit-lib` reshuffles its 16 codegen units, which can move a 2–3 ms command by ±50 µs and its peak RSS by ±1 MB with no change in the work done. Confirm small differences on `dist`.

| Flag | Meaning |
| --- | --- |
| `--smoke` | Smoke fixtures, **0** warmups, **1** timed run per tool |
| `--only id,...` | Run a subset of scenario ids |
| `--area <area>` | Filter by functional area (`worktree`, `branching`, `remotes`, …) |
| `--baseline-grit <path>` | Also time a second grit binary on headline rows |
| `--offline` | Do not clone `git.git` (fixtures must already be cached) |

All **17 headline** ids run in **`--smoke`** mode (including the four **remote** headlines on cached `git-git` bare and synth-10k bare remotes). Additional **remote matrix** rows (`fetch/noop`, `ls-remote/all`, HTTP clone/fetch/push) run on full daily reports and in `grit-utils` remote e2e tests. CI job **`bench-daily-smoke`** runs the smoke command above; timings are not gated.

## Blobless hard reset (#2054)

Measured on 2026-10-26 with the stock system Git 2.43.0, Rust 1.99.0, release-profile grit 0.5.21, four Intel Xeon vCPUs and 16 GiB RAM on Linux/ext4. The main baseline is `fa6a9fd559b2b5e0ac4779f1d2b4c115f70c24f0`; the comparison is the `factory/batch-reset-blobs` change. Both binaries were retained and measured in the same session, with no concurrent builds or tests.

Wall times below are **median milliseconds**, and RSS is the **maximum KiB** across samples. GNU `/usr/bin/time -v` measured peak client RSS; Python `time.monotonic()` bracketed the complete command. Clone preparation, object counting, byte/symlink verification, clean `git status --porcelain` and `git fsck --strict` ran outside the timed region. Every materialization used a fresh `--no-checkout` client. The 200-file case has three runs per tool; Cargo and HTTP blobless cases have one.

| Blobless materialization | Git: ms / KiB | Main: ms / KiB | Fixed: ms / KiB |
| --- | ---: | ---: | ---: |
| 200 files, file transport | 30.24 / 5,704 | 1,394.26 / 21,792 | 33.03 / 17,504 |
| 200 files, smart HTTP | 161.84 / 9,584 | 1,804.66 / 23,968 | 69.63 / 18,112 |
| Cargo, file transport | 678.57 / 45,136 | 72,193.59 / 41,644 | 757.40 / 45,344 |

| Case | Data fetches: Git / main / fixed | Total packs: Git / main / fixed | HTTP requests: Git / main / fixed |
| --- | ---: | ---: | ---: |
| 200 files | 1 / 200 / 1 | 2 / 201 / 2 | — |
| 200 files, HTTP | 1 / 200 / 1 | 2 / 201 / 2 | 2 / 400 / 2 |
| Cargo | 1 / 2,022 / 1 | 3 / 2,023 / 2 | — |

The local HTTP server was the same debug-build `grit-http-server` for all three clients, with `--log-headers` enabled. HTTP is a single sample and includes server warmup effects; request counts are the regression gate. File-transport fetch counts come from system-Git upload-pack traces. A second reset downloads nothing, as covered by library and CLI tests.

Cargo was cloned from `https://github.com/rust-lang/cargo.git` and its source branch/worktree reset to `b57863321f8d0a0f76fcf9ad636b6e39b024d0c0` before client creation. All three blobless clients started with **59,756 missing history blobs** and identical target contents; the current source tag set differs from the original issue reproduction. The verifier compares symlink targets without following directory symlinks.

Cargo blobless checkout improves about **95×**. Its peak RSS increases by about **3.6 MiB** relative to main and is close to system Git. The larger single fetch replaces thousands of tiny packs; this measured memory cost is retained in the baseline rather than hidden by the speedup. The 200-file and HTTP blobless cases use less peak memory than main.

Ordinary full-clone resets check the common path separately. Cargo values use four fresh clients per tool; each clean reset follows successful full materialization.

| Full-clone reset | Git: ms / KiB | Main: ms / KiB | Fixed: ms / KiB |
| --- | ---: | ---: | ---: |
| 200 files, materialize | 5.49 / 5,672 | 9.82 / 13,880 | 9.58 / 15,464 |
| 200 files, clean | 5.19 / 5,416 | 6.35 / 14,544 | 5.97 / 15,900 |
| Cargo, materialize | 149.66 / 46,100 | 180.57 / 56,740 | 178.71 / 56,860 |
| Cargo, clean | 14.03 / 15,300 | 17.92 / 20,384 | 17.35 / 19,976 |

Full Cargo materialization stays within the observed run-to-run range; clean Cargo resets improve slightly and use slightly less peak RSS. Tiny full-clone cases show about 1.3–1.6 MiB more peak RSS, so the results do not claim a universal memory reduction. The raw samples and environment are committed in `grit-utils/baselines/blobless-reset-before.json` and `grit-utils/baselines/blobless-reset-after.json`.

To reproduce the small input repository:

```bash
case_root=$(mktemp -d)
git init -q -b main "$case_root/source"
for i in $(seq 1 200); do
  { printf 'file %s\n' "$i"; for j in $(seq 1 100); do printf 'build payload\n'; done; } > "$case_root/source/file-$i.txt"
done
git -C "$case_root/source" add .
git -C "$case_root/source" -c user.name=CI -c user.email=ci@example.test commit -qm inputs
git -C "$case_root/source" config uploadpack.allowFilter true
grit clone --blobs none --no-checkout "file://$case_root/source" "$case_root/client"
/usr/bin/time -v grit -C "$case_root/client" reset --hard HEAD
git -C "$case_root/client" fsck --strict
```

Use a fresh destination for each Git/main/fixed sample, substituting `git clone --filter=blob:none --no-checkout` for Git. Omit the blob filter for full materialization; run reset again on the resulting clean worktree for the clean case. For Cargo, substitute the pinned source above. Keep the source upload-pack executable as the stock system Git.

The daily remotes matrix also measures materialization separately from clone time:

```bash
grit-bench daily --only reset/blobless@git-git --output target/bench/blobless-reset.json
grit-bench daily validate target/bench/blobless-reset.json
```

Its prepare hook creates fresh blobless clients outside each timed iteration. Correctness checks enforce clean worktrees, strict fsck and a bounded pack count.

## Startup relocation comparison

Startup changes need separate `release` and shipping `dist` measurements. Keep
PIE, ASLR, LTO, codegen units and panic policy unchanged. A CLI-only RELR probe
changes the final link, without rebuilding dependencies with workspace-wide flags:

```bash
mkdir -p /tmp/grit-startup/{release,dist}/{stock,relr}
cargo build --release -p grit-cli --locked
cp target/release/grit /tmp/grit-startup/release/stock/grit
cargo rustc --release -p grit-cli --locked --bin grit -- \
  -C link-arg=-Wl,-z,pack-relative-relocs \
  -C link-arg=-Wl,-Map=/tmp/grit-startup/release-relr.map
cp target/release/grit /tmp/grit-startup/release/relr/grit
cargo build --profile dist -p grit-cli --locked
cp target/dist/grit /tmp/grit-startup/dist/stock/grit
cargo rustc --profile dist -p grit-cli --locked --bin grit -- \
  -C link-arg=-Wl,-z,pack-relative-relocs \
  -C link-arg=-Wl,-Map=/tmp/grit-startup/dist-relr.map
cp target/dist/grit /tmp/grit-startup/dist/relr/grit
cargo build -p grit-utils --bin grit-bench --example startup_compare
cc -O2 grit-utils/examples/startup_measure.c -o /tmp/grit-startup/measure
```

Stop builds and tests before timing. The focused example uses the daily runner's
`measure-direct-run` helper and statistics, with five warmups and **60 measured
runs per tool** (minimum 30), rotating stock/candidate/system-Git order. The daily
default policy caps at 20 and is insufficient for this investigation.

```bash
./target/debug/examples/startup_compare \
  --stock /tmp/grit-startup/dist/stock/grit \
  --candidate /tmp/grit-startup/dist/relr/grit \
  --helper "$PWD/target/debug/grit-bench" \
  --native-helper /tmp/grit-startup/measure \
  --fixtures /tmp/grit-startup/fixtures --profile dist \
  --output /tmp/grit-startup/dist.json
```

Repeat with the frozen `release` binaries and `--profile release`. Fixtures cover
one local branch, 20,000 refs, 20,000 tracked paths, and 20,000 commits. Every
fixture passes system `git fsck --strict`; each candidate command must preserve
stock stdout/stderr. Branch, status, resolve, help and a 1,000-entry log run on
all four shapes. Git's branch list is unbounded while grit's default list is
bounded; the comparison reports the actual argv and does not equate their output
volume on the many-ref fixture.

Reports retain individual wall-time samples, medians, standard deviations and
per-child peak RSS. The existing helper measures after spawn, through child
exit; the small C parent records full fork/exec/wait times, per-child RSS and page faults in the `native` block for startup conclusions.
Cross-check RSS with `/usr/bin/time -v`, using a fresh small parent per child;
a long-lived Python parent's inherited RSS can swamp short-command measurements.
Record minor/major page faults separately, and keep `LD_DEBUG=statistics` loader
cycles apart from end-to-end wall time. Neither loader cycles nor relocation
metadata bytes measure heap use.

Inspect `readelf -hW -lW -dW -rW -VW`, `nm -S -n`, and the linker maps. RELR
compresses relative relocation metadata; it still writes the same pointer slots.
Dependency tables, including regex Unicode tables, contribute relocations even
when their code executes lazily. Preserve Unicode matching, filtering, schemas
and tracing when considering source changes.

The release workflow packages the original glibc 2.34 x86_64 GNU asset before
performing this CLI-only RELR link for a separate `-gnu-relr` asset. The latter
requires glibc 2.36+ and remains a manual download. Stable/nightly installers
always use the original GNU asset on glibc systems: the final-tip review found
a 1–2% startup regression, despite earlier gains. Before enabling automatic
selection, benchmark stock and RELR from the exact release source again with
builds/tests stopped; require a demonstrated benefit rather than inferring one
from compressed metadata or loader support. This preserves the existing loader
minimum and leaves other targets unchanged. Run both ELF/runtime checks and the
pinned 2.34/2.36 container smokes in `.github/workflows/release.yml`; see
[TESTING.md](https://github.com/gitbutlerapp/grit/blob/main/TESTING.md#linux-gnu-startup-and-loader-contract).
The initial measured dist gain is 5–8% for tiny commands. The earlier Xeon
shipping follow-up measured 4–5% for branch/status/resolve, with help within
its sample spread. Both reports retain variance and independent RSS; layout
changes mean metadata compression is not a general memory reduction. The
shipping JSON also records 30 loader-only observations and ten GNU-time peak
RSS samples per tiny command/tool.

The final-tip follow-up in `startup-relocations-review-{release,dist}.json`
records exact `3fcfb24e9` CLI/library source, frozen binary hashes and the actual
AMD EPYC 7763 host. Tiny dist commands regress 0.88–2.07%; release changes are
near zero. The native block includes paired-bootstrap 95% intervals from 5,000
resamples of the 60 round indices (reset RNG seed 4626 per case), taking
`median(candidate) - median(stock)`. Loader-only and GNU-time cross-checks use
the runner's isolated environment, recorded in `isolated_env_overrides`.
