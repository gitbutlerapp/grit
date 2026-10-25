# 2026-10-24 daily regressions: before and after

`daily-perf-4262-before.json` and `daily-perf-4262-after.json` are `grit-bench daily --only <id>` runs from one sitting, before and after interleaved per scenario, on the same fixtures with system git 2.43.0 on a 4-vCPU Intel Xeon VM (release profile). "Before" is `origin/main` at eb67993a0, and "after" is that commit plus four changes:

- `add` keeps the tracked-scan stat preload parallel on indexes of 80k+ entries, and restages changed blobs in parallel, in chunks of at most 1024 paths and 64 MiB whose stage-0 rows are overwritten in place.
- `add` reads each restaged file once: hash, then store through `Odb::write_blob_prehashed`, which skips compression when the object already exists. Entries this process just hashed skip the pre-write racy re-hash, as Git's `CE_UPTODATE` entries do.
- `log -n` on the cursor walk reads each listed commit once: the peel returns the object it read, and `LogWalkPage::commit_data` hands the walk's parsed commits to the renderer.
- A protocol v2 fetch walks local history for `have` lines only as far as the rounds send them, instead of draining the negotiator over all of git.git first.

The network rows moved on 2026-10-24 because the harness switched from `file://` to git http-backend that day. The 2026-10-23 grit binary is as slow as the 2026-10-24 one under the new harness (fetch/dry-run 785 ms vs 759 ms), so those rows reflect harness changes, not grit regressions. The 7x HTTP fetch gap was real, though, and the lazy haves change closes it.

| scenario | git ms | grit before ms | grit after ms | grit/git before → after | grit peak RSS KB before → after |
| --- | ---: | ---: | ---: | --- | --- |
| `add/update@synth-100k` | 105.9 | 203.3 | 95.6 | 1.94× → 0.90× | 39980 → 42812 |
| `add/modified@big-blobs` | 218.5 | 490.4 | 172.6 | 2.24× → 0.79× | 158960 → 159088 |
| `add/all@synth-100k` | 152.7 | 252.6 | 136.6 | 1.68× → 0.88× | 46772 → 46960 |
| `commit/small@synth-100k` | 942.2 | 313.6 | 195.5 | 0.34× → 0.20× | 45284 → 47276 |
| `log/all@git-git` | 46.7 | 91.1 | 63.6 | 1.93× → 1.38× | 37460 → 41544 |
| `fetch/dry-run@git-git` | 115.5 | 810.0 | 56.3 | 6.88× → 0.50× | 65752 → 48932 |
| `fetch/incremental@git-git` | 111.4 | 748.8 | 53.9 | 6.79× → 0.48× | 66212 → 49024 |
| `fetch/one-branch@git-git` | 116.0 | 760.0 | 54.4 | 6.40× → 0.48× | 65628 → 49056 |
| `pull/fast-forward@git-git` | 126.2 | 748.5 | 60.4 | 6.15× → 0.46× | 66452 → 49068 |
| `pull/check@git-git` | 115.8 | 742.2 | 58.8 | 6.44× → 0.51× | 66364 → 49204 |

The other rows in the reports (the rest of `add`, `commit`, `log`, `fetch/noop`, `pull/rebase`, `status`, `stash`, `clone/local` and `push/incremental`) are within run-to-run noise. Two reruns each of `commit/staged@synth-100k` and `status/dirty@synth-100k` came out faster once and slower once.

Outside the harness, a bulk `add -u` with all 100k files modified takes 2.9 s at 52 MB peak RSS (git: 3.2 s / 66 MB; before: 4.6 to 11 s / 52 MB). Adding two large blobs whose content is not yet stored takes 1.47 s (git: 2.47 s; before: 1.91 s).
