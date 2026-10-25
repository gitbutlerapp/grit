# `show` HEAD: before and after

`daily-show-head-before.json` and `daily-show-head-after.json` are `grit-bench daily` runs from one sitting, with the same fixtures and the same system git 2.43.0 on an Intel Xeon VM (release profile). "Before" is `origin/main` at 77fd8a48c; "after" is that commit plus four changes:

- `show` builds ref decorations only for `--json`, the only output that renders `commit.refs`
- single-entry content diffs run serially without resolving the thread policy (which reads the cgroup CPU quota)
- the virtual empty tree is recognized by its known ids instead of hashing `tree 0\0` before every object read
- the cached local config text is matched by path before falling back to canonicalizing both paths

The daily harness polls each child with a 1 ms sleep, so its wall times move in steps of about 1.09 ms, and every `show` row stays in its bucket. The table below uses `hyperfine -N` medians (at least 300 runs) on the same commands and fixtures. Peak RSS is the hyperfine median.

| scenario | git ms | grit before ms | grit after ms | change | ratio before → after | peak RSS KB git / before / after |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| `show/head@git-git` | 1.71 | 3.51 | 3.15 | -10.3% | 2.05× → 1.84× | 6828 / 16304 / 14800 |
| `show/head@big-blobs` | 1.31 | 2.62 | 2.32 | -11.5% | 2.00× → 1.77× | 5444 / 14724 / 13320 |
| `show/head@synth-10k` | 1.71 | 3.57 | 3.16 | -11.6% | 2.08× → 1.84× | 6680 / 16478 / 15256 |
| `show --json` (git.git) | 1.63 | 3.10 | 2.95 | -4.7% | 1.90× → 1.81× | 6828 / 16372 / 15196 |
| `log/oneline-100@git-git` | 2.74 | 4.13 | 4.00 | -3.2% | 1.51× → 1.46× | 12604 / 21456 / 20604 |
| `cat/blob@git-git` | 1.14 | 2.19 | 2.12 | -3.2% | 1.92× → 1.86× | 5148 / 12640 / 11792 |
| `resolve HEAD~5` (git.git) | 1.14 | 2.32 | 2.28 | -2.0% | 2.04× → 2.00× | 5740 / 13348 / 12592 |
| `diff/worktree@synth-10k` | 27.97 | 24.31 | 23.12 | -4.9% | 0.87× → 0.83× | 6904 / 24998 / 24502 |
| `status/clean@synth-10k` | 6.78 | 8.44 | 8.28 | -1.9% | 1.24× → 1.22× | 7128 / 16482 / 15772 |
| `log/stat-n50@git-git` (30–40 runs) | 96.93 | 331.87 | 322.35 | noise | 3.42× → 3.33× | 37492 / 106426 / 105460 |

What is left of the `show` gap is mostly process cost rather than work. Half of grit's run time is in the kernel, and two thirds of that is page faults: about 130 copy-on-write faults from applying 31.7k PIE relative relocations to `.data.rel.ro` at every start, about 135 faults spread over a 24 MB text segment (each maps a 64 KB window, which is most of the RSS gap), and about 130 heap faults for a 540 KB heap peak.
