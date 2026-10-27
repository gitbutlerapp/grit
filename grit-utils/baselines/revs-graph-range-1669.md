# Graph-backed revision range counts (#1669)

Baseline: `1c4fdeca7dd39930aa0c8e90b3d902b23febb5ca` (`origin/main` at checkout). Fixed code: `9a30c347e`. Both binaries were built with `cargo build --profile dist -p grit-cli`, Rust 1.99.0, on the same Ubuntu 24.04.5 x86_64 VM (four vCPUs, AMD EPYC 7763). The control is unmodified system Git 2.55.0.

## Reproduction and method

The upstream git/git clone has 85,828 commits reachable from all refs. After creating `topic` at `master~20`, its tip is `583e98cd091c78e22450a000c163d98ca4bc2b20` with 82,537 ancestors. `master..topic` is empty; `HEAD~20..HEAD` contains 56 commits because of merges. Git and both grit binaries agree.

Count and listing were measured with no graph, a graph written by `git commit-graph write --reachable`, and one written by `grit gc commit-graph`. Graph generation was outside the timed region. The reader continues to map the graph; the fix avoids materializing the shared ancestry.

Hyperfine 1.19.0 used `-N --warmup 5 --runs 30`. Processes were pinned to CPU 3; remaining optional harness tests were restricted to CPUs 0–1 on the other physical core. Each RSS value is the median of five independent `/usr/bin/time -v` process peaks. The table reports means, not one-off stopwatch measurements.

```bash
git clone --bare https://github.com/git/git gitgit.git
git clone -q gitgit.git g
cd g
git switch -c topic HEAD~20
# Set GRIT_RANGE_MAIN and GRIT_RANGE_FIXED to the two dist binaries.
hyperfine -N --warmup 5 --runs 30 \
  "git rev-list --count HEAD~20..HEAD" \
  "$GRIT_RANGE_MAIN revs --count HEAD~20..HEAD" \
  "$GRIT_RANGE_FIXED revs --count HEAD~20..HEAD"
/usr/bin/time -v "$GRIT_RANGE_FIXED" revs --count HEAD~20..HEAD
# Repeat without --count, and with HEAD instead of the range as a control.
# Repeat after each graph writer; remove objects/info/commit-graph between setups.
```

## Wall time and peak memory

Each cell is **mean milliseconds / median peak MiB**. `count` and `list` use `HEAD~20..HEAD`; `count-head` counts the full `HEAD` history.

| Graph | Operation | System Git | Main | Fixed |
| --- | --- | ---: | ---: | ---: |
| no-graph | count | 6.757 / 15.75 | 22.597 / 22.56 | 23.340 / 22.48 |
| no-graph | list | 7.895 / 15.79 | 9.012 / 20.88 | 8.860 / 20.78 |
| no-graph | count-head | 629.073 / 65.55 | 877.348 / 125.10 | 887.664 / 125.05 |
| git-graph | count | 2.424 / 10.88 | 72.763 / 15.67 | 3.684 / 14.57 |
| git-graph | list | 2.500 / 10.94 | 5.215 / 19.33 | 5.353 / 19.27 |
| git-graph | count-head | 43.684 / 19.64 | 48.610 / 20.14 | 48.260 / 20.37 |
| grit-graph | count | 2.429 / 10.97 | 74.928 / 15.76 | 3.717 / 14.77 |
| grit-graph | list | 2.527 / 11.04 | 5.037 / 19.25 | 5.134 / 19.28 |
| grit-graph | count-head | 43.854 / 19.74 | 47.887 / 20.34 | 47.492 / 20.34 |

With a Git-written graph, range count improves **19.8×** and takes **1.52×** Git’s time; median peak RSS falls from **15.67 to 14.57 MiB** (Git: **10.88 MiB**). The grit-written graph improves **20.2×**, reaching **1.53×** Git’s time.

Listings and full-history controls stay within 3.3% in mean time and 0.25 MiB in RSS. A second check alternated main/fixed and fixed/main for 20 pairs without a graph: range count averaged 23.941 / 23.788 ms (standard deviations 3.992 / 3.365 ms); full-history count averaged 863.328 / 875.880 ms (17.451 / 29.827 ms). These samples show no clear regression at this precision.

## Daily regression scenario

`revs/count-range-commit-graph@git-git` explicitly writes and verifies a Git commit-graph before timing the bounded count. The committed daily before/after JSON files use the canonical `v2.47.0` fixture, 30 samples per tool, and include the raw samples and peak RSS. Both reports pass `grit-bench daily validate`.

```bash
grit-bench --grit /path/to/grit --min-runs 30 daily \
  --only revs/count-range-commit-graph@git-git \
  --output range-count.json
grit-bench daily validate range-count.json
```

## Validation and existing blockers

- The structural regression failed before the fix: 6,001 parent lookups through `RevWalk::count` and 5,980 through `rev_list` for a 20-commit range in a 3,000-commit chain. After the fix both satisfy the 60-lookup bound.
- All 28 bounded-walk integration tests pass, including Git/grit graph writers, Git fsck, empty/reversed ranges, merges, multiple tips/exclusions and skewed dates. The 59 revision ordering, bitmap and simplification tests also pass. Daily benchmark unit tests: 31 passing.
- Full library unit runs each report 2,669 passed, 3 failed, 3 ignored. Before: the intentionally failing regression plus two reset/Git-maintenance races. After: the regression passes; three reset tests hit Git fsck errors involving concurrently removed temporary pack/loose files. All 11 hard-reset tests pass when Git automatic maintenance is disabled through process configuration.
- Required `cargo test -p grit-lib -p grit-cli` was run before and after. Both stop at the existing `history_injected_ref_store.rs:45` compile error (`Signature: From<&str>`). `make gate` passes fmt, workspace Clippy with warnings denied, workspace rustdoc with warnings denied and the public-module doctest gate, then stops at the same compile error.
- Separate CLI validation: 155 unit tests pass, with the existing command-page template failure in unchanged blame/repack/patch-id pages. Integration binaries report 457 passing tests and one JSON error test failure; the failing empty-repository command produces identical output with the saved main and fixed binaries.
- The optional broad daily E2E run was stopped while running unrelated full-repository maintenance/fsck workloads. It had reported an area-filter failure and a passing baseline-headline test. The new scenario itself executed successfully and both its committed reports validate. No unrelated test or implementation was changed.
- Documentation content validation passes. No public API signatures or CLI outputs change.
