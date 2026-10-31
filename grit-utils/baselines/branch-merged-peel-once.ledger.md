# `grit branch --merged` on many-refs: profile and peel-once filter

Main: `3c14b849f62053bb6aa30bdcab3a22abea143192`. Measured 2026-10-31 on a four-vCPU Intel
Xeon VM, Linux 6.12, system Git 2.43.0, Rust 1.99.0, Cargo `release` profile. Daily JSON:
`daily-branch-merged-peel-once-{before,after}.json` (30 timed runs per tool per scenario).

## Where the gap to Git comes from

The many-refs fixture has 11,668 `packed-refs` lines (6,667 `refs/remotes/*`, 3,333
`refs/tags/*` with peel lines) and a single loose local branch, `refs/heads/main`.
`grit branch --merged main` therefore lists one branch, and the listing already
binary-searches `packed-refs` for the `refs/heads/` range instead of loading every ref.

| Measurement (`branch --merged main`) | grit | git |
| --- | ---: | ---: |
| Wall time, hyperfine median of 400 | 2.10 ms | 1.1 ms |
| Peak RSS | 15.4 MiB | 5.4 MiB |
| Minor page faults | 706 | 453 |
| Instructions (callgrind) | 2.40 M | — |
| Heap at peak (DHAT) | 107 KB | — |

At exit, 12.8 MiB of grit's RSS is pages of the 45 MB `grit` executable itself (10.3 MiB
of `.text`). For comparison, `grit --version` costs 1.3 ms, 6.5 MiB and 618 faults, and
plain `grit branch` costs 2.0 ms and 13.6 MiB. The peak RSS gap is code paged in from the
binary, not data structures. Heap peaks at 107 KB, and no pack, index or `packed-refs`
mapping stays resident.

Of the 2.40 M instructions, 0.80 M (33%) are the dynamic loader applying relocations
before `main`. Repository discovery and the config cascade take 0.47 M, clap parsing and
command construction about 0.30 M, and environment capture 0.10 M. `--merged` itself adds
0.25 M over plain `grit branch` (about 60 µs):

- 77 K: the CLI resolving `main` (DWIM probes into `packed-refs`).
- 114 K: the first object lookup, which scans the pack directory and multi-pack-index once.
- About 23 K: redundant header reads. Each tip was peeled before filtering, peeled again to
  find its commit, and the merged target was read a third time.

The reachability walk is negligible: the tip is the base. That leaves the fixed costs
every `branch/*` and `tag/*` row shares, outside the ref-listing path: relocation count,
config loading, clap construction and code layout across a large text segment.

## Change

Reachability-filtered queries (`--merged`, `--no-merged`, `--contains`, `--points-at`) no
longer peel every listed ref up front. The filter resolves each distinct tip once and
records its tag peel on the row. The listing applies a keep mask instead of cloning every
ref name into a `HashSet<String>`, and peels only kept rows the filter never resolved.
`packed-refs` peel lines are still consulted first when 64 or more refs need a peel.
`branch_tips_merged_into` returns without building the commit cache or ranking tips when
every tip is a merge target.

## Results

Instruction counts (callgrind, deterministic) on the many-refs fixture:

| Command | Before | After | Change |
| --- | ---: | ---: | ---: |
| `branch --merged main` | 2,398,104 | 2,373,614 | −1.0% |
| `branch --merged HEAD` | 2,347,239 | 2,323,852 | −1.0% |
| `branch -a --merged main` (6,668 refs) | 122,378,400 | 114,776,191 | −6.2% |
| `branch -a --no-merged main` | 95,756,284 | 92,887,795 | −3.0% |
| `branch -r --contains main` | 120,646,212 | 116,050,184 | −3.8% |
| `branch` (no filter) | 2,152,301 | 2,150,232 | −0.1% |
| `branch --json` (no filter) | 2,355,625 | 2,354,962 | 0.0% |
| `tag` (no filter) | 26,860,958 | 26,830,948 | −0.1% |
| many-refs-loose `branch --merged main` | 2,400,213 | 2,376,073 | −1.0% |

Hyperfine, 60 runs: `branch -a --merged main` went from 13.2 to 12.7 ms, and
`branch -a --no-merged main` from 11.0 to 10.5 ms. Git takes 26.5 ms on the first.

Daily matrix medians (ms) and peak RSS (KiB), from the committed JSON:

| Scenario | Git ms | Grit before ms | Grit after ms | Git KiB | Grit before KiB | Grit after KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| branch/create@many-refs | 1.495 | 2.673 | 2.759 | 7,588 | 18,124 | 18,516 |
| branch/delete@many-refs | 3.858 | 4.370 | 4.647 | 7,848 | 17,340 | 17,524 |
| branch/list-json@many-refs | 1.589 | 2.378 | 2.387 | 7,696 | 17,272 | 16,232 |
| branch/list@many-refs | 1.270 | 2.202 | 2.299 | 6,224 | 16,064 | 15,404 |
| branch/list@many-refs-loose | 1.372 | 2.321 | 2.244 | 6,804 | 14,432 | 14,316 |
| branch/merged-ref@many-refs | 0.748 | 1.936 | 1.801 | 6,196 | 11,488 | 11,488 |
| branch/merged@many-refs | 1.459 | 2.358 | 2.433 | 7,472 | 17,440 | 16,224 |
| branch/rename@many-refs | 1.671 | 3.486 | 3.440 | 7,572 | 12,828 | 12,704 |
| tag/create@many-refs | 1.536 | 2.733 | 2.685 | 7,480 | 17,656 | 18,120 |
| tag/list@many-refs | 3.405 | 4.622 | 4.323 | 7,920 | 16,600 | 16,804 |

On single-branch rows the change is smaller than the noise floor. `branch/create`,
`branch/delete` and `tag/create` never reach the changed code but still moved by up to
±0.28 ms. Copying the same `grit` binary to a new file shifted
`branch --json` by 0.06 ms with an identical fault count, a page-cache placement
effect. The instruction counts above are the reliable before/after signal for those rows.

## Follow-ups outside this change

The remaining ~1 ms / ~9 MiB gap to Git on `branch/*` single-branch rows is fixed startup:

- Dynamic relocations (0.8 M instructions per run; see `startup-relocations.md`).
- The config cascade and repository discovery (0.47 M).
- Building the full clap command tree for one subcommand (about 0.3 M).
- Code layout across the 26 MB `.text` segment (about 10 MiB of it paged in per command).
