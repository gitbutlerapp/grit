# Log page and startup: before and after

`daily-log-startup-before.json` and `daily-log-startup-after.json` are `grit-bench daily` runs from one sitting. Both use the same fixtures and the same system git 2.43.0 on an Intel Xeon VM. "Before" is `origin/main` at 29bf85181; "after" is that commit plus three changes:

- deferred clap subcommand building
- lean retained `log` page walk
- single-pack reader reuse across object reads

The daily harness polls each child with a 1 ms sleep, so its wall times move in steps of about 1.09 ms. Most rows here are 2 to 6 ms, so a sub-millisecond change shows up only when it crosses a step. For example, `log/oneline-100@git-git` stays in the 5.45 ms bucket while `log/oneline-100@synth-100k` drops from 5.50 to 4.35 ms. The table below uses `hyperfine -N` medians on the same commands and fixtures, which do not have that rounding. Peak RSS is the maximum of five `/usr/bin/time` runs.

| scenario | git ms | grit before ms | grit after ms | change | ratio before → after | peak RSS KB git / before / after |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| `log/oneline-100@git-git` | 2.73 | 4.58 | 3.89 | -15.0% | 1.68× → 1.43× | 12700 / 21920 / 20892 |
| `log/json-100@git-git` | 2.83 | 5.03 | 4.44 | -11.8% | 1.78× → 1.57× | 12680 / 22680 / 21536 |
| `log/all@git-git` | 46.05 | 67.16 | 57.17 | -14.9% | 1.46× → 1.24× | 18708 / 38968 / 37404 |
| `log/author@git-git` | 2.14 | 4.61 | 4.06 | -11.9% | 2.16× → 1.90× | 10420 / 20816 / 19888 |
| `log/author-n50@deep-history` | 1.46 | 3.74 | 3.23 | -13.6% | 2.55× → 2.21× | 7072 / 17464 / 16468 |
| `log/oneline-100@synth-100k` | 1.98 | 3.70 | 3.16 | -14.6% | 1.87× → 1.60× | 11216 / 20260 / 18908 |
| `log/path@git-git` | 359.70 | 2464.97 | 2372.91 | -3.7% | 6.85× → 6.60× | 254244 / 312344 / 311716 |
| `log/limit-path@git-git` | 20.40 | 54.61 | 51.86 | -5.0% | 2.68× → 2.54× | 31044 / 39616 / 39160 |
| `log/json-filter@git-git` | 2.03 | 4.93 | 4.52 | -8.2% | 2.43× → 2.23× | 10396 / 21336 / 20116 |
| `log/stat-n50@git-git` | 96.13 | 407.52 | 407.03 | -0.1% | 4.24× → 4.23× | 37392 / 119704 / 119912 |
| `show/head@git-git` | 1.68 | 3.73 | 3.12 | -16.5% | 2.22× → 1.85× | 6944 / 16996 / 16488 |
| `resolve HEAD~5` (git.git) | 1.24 | 2.91 | 2.35 | -19.2% | 2.34× → 1.90× | 5760 / 14468 / 13032 |
| `revs/count@git-git` | 547.06 | 794.32 | 722.52 | -9.0% | 1.45× → 1.32× | 59444 / 125524 / 124608 |
| `status/clean@synth-10k` | 7.63 | 10.89 | 9.63 | -11.6% | 1.43× → 1.26× | 7208 / 17240 / 16568 |
| `--version` (small) | 0.83 | 2.03 | 1.24 | -39.1% | 2.45× → 1.49× | 4012 / 7516 / 5724 |
