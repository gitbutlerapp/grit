# Task #4687: log statistics performance — ready for review

Branch: `factory/log-stat`, restored on mesh `grit-perf-4687-r2:grit` from `grit-perf-4687`. Code commit: `f3c83c08a9452b042636f1f7432bb82c967acd03`; baseline: `f2a97fc88fe49fc0ccba9bfdb7f3b2a4b47a4513`.

The target improves **385.2 → 192.5 ms** against stock Git **103.1 ms** (10-run hyperfine means). Direct peak RSS improves **115,248 → 99,988 KiB**, versus Git **36,376 KiB** (maximum of three fresh-process runs). This is about **50.0% less time and 13.2% less target RSS**. Startup-heavy commands use about 1 MiB more RSS. The maintainer explicitly approved this measured, disclosed tradeoff on 2026-10-27 and directed proceeding to review, with short-command timing checks beside the adaptive table and a separate memory follow-up. No push or integration was performed.

## Root cause and change

The stat counter tokenized whole files and built a large generic comparison table, then iterated every unchanged line while counting. Blob preparation copied complete buffers and UTF-8 strings, and log computed a second full patch only to populate its summary. The library now trims equal boundary lines, interns borrowed LF lines into compact ids for the same Myers engine, borrows valid UTF-8 and transfers owned blob buffers; the CLI renders the statistics already computed by the library.

CR-containing text retains the previous tokenizer, including CR/CRLF behavior. Other diff algorithms retain their existing paths. JSON `stat` now uses the same paths/counts as `files` and `totals`; the redundant patch wrapper is deleted. Public API signatures are unchanged.

## Validation

### Original validation on 2026-10-26

- `cargo test -p grit-lib -p grit-cli`, before and after: compilation blocked by the unchanged E0277 in `grit-lib/tests/history_injected_ref_store.rs:45` (`Signature: From<&str>` missing). Tests were not weakened.
- Separate library unit suites: before **2600 passed / 1 failed / 3 ignored**; after **2601 passed / 2 failed / 3 ignored**. Failures are reset fixtures racing system Git maintenance during `git fsck`: baseline `hard_reset_keeps_recorded_stat_rows_for_unchanged_files`; final `hard_reset_delete_heavy_reads_no_worktree_files_and_stats_each_once` and `hard_reset_rewrites_racy_unchanged_file_with_same_size_edit`. The latter passes individually; deletion-heavy rerun still encounters temporary/disappearing pack files. An intermediate full library run passed 2603 tests, illustrating the existing nondeterminism.
- CLI unit suite: **156 passed before and after**. Selected log/show/shortlog/output-snapshot integration suites: **148 before / 149 after**, all passing.
- New public-library packed-history round trip: **1 passed**. Grit-written commits/trees/blobs pass system `git fsck --strict`, Git writes an additional commit, Git repacks/prunes loose objects, and serial/parallel batched statistics match `git diff-tree --numstat`. Covers small edits in large files, root commits, binary and invalid UTF-8 content, mode changes, add/delete and path filtering. Existing CLI tests cover renames and merges.
- New unit coverage compares edge cases and 500 deterministic randomized inputs with the previous counter, including repeated lines, Unicode, CR/CRLF and missing final newline. CLI regression checks JSON summaries, paths, binary files, renames and Markdown.
- `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, benchmark/integration Clippy with `http-ureq,tracing`, and rustdoc pass. Docs check passes: 93 pages, 50 commands, 7 blog posts.

### Fresh review validation

The 2026-10-27 `cargo test -p grit-lib -p grit-cli` attempt stops during compilation with 14 pre-existing E0063/E0308 errors in `grit-lib/tests/serve_upload_pack_inprocess.rs`: fixture `ServeOptions` initializers omit `server_options`, and `upload_pack` expects a mutable reference. Both this test and `grit-lib/src/serve/upload_pack.rs` are byte-identical to baseline `f2a97fc88`; none of the server code is changed by #4687. Keep this additional baseline failure separate from the previously reported E0277 and reset/fsck flake.

Fresh `cargo test -p grit-lib --lib`: **2602 passed / 1 failed / 3 ignored**. The sole failure is the already recorded `hard_reset_rewrites_racy_unchanged_file_with_same_size_edit`, again in system `git fsck` while loose objects disappear during fixture maintenance. The new diff-counting unit tests pass. This is the baseline reset/fsck flake, not a #4687 regression.

Fresh `cargo test -p grit-cli --bin grit --test log_diff_detail --test log_display_regressions --test log_display_snapshots --test log_filter_cli --test log_pickaxe_json --test log_author_name --test show_log_patch_text --test show_p1_2384 --test show_format_flags --test shortlog_target --test shortlog_flags_cli --test output_snapshots`: **156 unit + 190 integration tests passed**, no failures or ignores. This fresh selection is broader than the original 149-test integration selection, so the counts are kept separate.

Fresh `cargo test -p grit-lib --test commit_files_many_git_compat`: **1 passed**, including system Git read/fsck and the reverse packed-history read. Targeted `cargo test -p grit-lib --test history_injected_ref_store` reproduces the unchanged **E0277 at line 45**. The pre-existing E0277 and reset/fsck failures remain explicitly documented as baseline failures.

Fresh `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`, `git diff --check`, and `cd site && npm run check` pass (93 pages, 50 commands, 7 blog posts). No tests were changed, newly skipped, weakened or removed during this review recheck. The full gate is not green because of the baseline failures above; the separate integrator still needs a green gate after rebasing onto repaired main.

## Measurements

Same 4-logical-CPU AMD EPYC 7763 VM, 16 GiB RAM, Rust 1.99 release profile and **stock Git 2.55.0**. The git.git fixture is pinned to v2.47.0 / `777489f9e09c8d0dd6b12f9d90de6376330577a2`. Binaries are frozen from the baseline and code commit. Builds/tests are idle for all published measurements. The first cache-population/contended baseline run was discarded; setup time is excluded.

The table uses adaptive daily-harness medians and helper-inclusive peak RSS. The Git column is the paired after-run Git control; both raw reports retain every Git sample. Primary target means above use 10 measured runs per tool after two warmups; direct RSS above excludes the helper. All eleven log scenarios and eight related scenarios were exercised.

| Scenario | Git ms | Grit before ms | Grit after ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| diff/staged@deep-tree | 61.53 | 52.45 | 52.97 | 48096 | 46144 |
| diff/staged@synth-100k | 101.18 | 70.49 | 74.09 | 69764 | 67424 |
| diff/stat@synth-100k | 186.77 | 164.42 | 157.48 | 59444 | 60668 |
| log/all@git-git | 55.63 | 77.15 | 78.43 | 36688 | 37740 |
| log/author-n50@deep-history | 2.99 | 5.87 | 6.29 | 18516 | 19456 |
| log/author-n50@deep-history-cg | 3.93 | 5.73 | 6.32 | 20800 | 21816 |
| log/author@git-git | 3.75 | 7.11 | 7.55 | 22496 | 23460 |
| log/json-100@git-git | 5.39 | 7.47 | 8.67 | 24116 | 25224 |
| log/json-filter@git-git | 3.74 | 7.03 | 7.69 | 23044 | 23820 |
| log/limit-path@git-git | 27.34 | 23.21 | 23.81 | 37516 | 38824 |
| log/oneline-100@git-git | 4.77 | 6.37 | 7.10 | 23480 | 24460 |
| log/oneline-100@synth-100k | 3.86 | 5.05 | 5.60 | 17812 | 18948 |
| log/path@git-git | 390.75 | 477.01 | 479.06 | 274696 | 275780 |
| log/stat-n50@git-git | 107.62 | 388.46 | 199.17 | 121396 | 104488 |
| shortlog/ahead@git-git | 21.63 | 3.55 | 3.75 | 12280 | 13376 |
| shortlog/behind@git-git | 31.06 | 3.59 | 4.20 | 12240 | 13724 |
| show/head@big-blobs | 3.02 | 4.07 | 4.54 | 15692 | 17484 |
| show/head@git-git | 3.01 | 4.35 | 4.99 | 17356 | 18844 |
| show/head@synth-10k | 3.23 | 5.61 | 5.74 | 17800 | 19768 |


### Review recheck on 2026-10-27

The maintainer's requested timing check used a fresh four-logical-CPU AMD EPYC 9V45 VM (16 GiB RAM), Rust 1.99 `release`, stock Git 2.55.0, and the same pinned git.git fixture. Both original source snapshots were rebuilt in this checkout and frozen before timing; no implementation changes or rebasing were made. These are separate same-sitting comparisons, not numbers mixed across the two VMs.

Fresh adaptive daily medians (9 runs/tool before and 15 after for the target, 20 for each short command; the primary direct target comparison below uses 20/tool; helper-inclusive RSS):

| Scenario | Git ms | Grit before ms | Grit after ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| log/author-n50@deep-history | 1.706 | 3.227 | 3.159 | 18388 | 18336 |
| log/author-n50@deep-history-cg | 1.983 | 3.235 | 3.322 | 20224 | 20220 |
| log/json-100@git-git | 2.691 | 4.012 | 3.987 | 24968 | 25372 |
| log/stat-n50@git-git | 75.756 | 276.079 | 135.126 | 118508 | 105296 |

Randomized interleaved direct timings, immediately beside the adaptive results: five warmups/tool, **100 samples/tool for each short command**, 20 for the target, fixed seed 4687, fresh processes, stdout discarded, no concurrent builds/tests. RSS is the maximum of five separate `/usr/bin/time -v` runs/tool and excludes the helper.

| Scenario | Git median ms | Before median ms | After median ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| log/json-100@git-git | 2.299 | 3.650 | 3.625 | 22764 | 23104 |
| log/author-n50@deep-history | 1.307 | 2.819 | 2.806 | 17624 | 17720 |
| log/author-n50@deep-history-cg | 1.546 | 2.914 | 2.901 | 19752 | 20092 |
| log/stat-n50@git-git | 75.054 | 263.902 | 133.768 | 120204 | 101124 |

The original adaptive JSON **7.47 → 8.67 ms** and author-n50 **5.87 → 6.29 / 5.73 → 6.32 ms** increases do not persist under randomized interleaving. Their original Git controls also moved **4.78 → 5.39**, **2.68 → 2.99**, and **3.01 → 3.93 ms**, respectively. In the fresh paired data, the 95% bootstrap intervals for the median within-round after-minus-before differences are JSON **[−0.026, +0.001] ms**, author without graph **[−0.016, −0.001] ms**, and author with graph **[−0.027, +0.004] ms**; there is no evidence of a real short-command timing regression. These checks support treating the original apparent slowdowns as noise; they do not claim universal timing improvements.

The startup RSS increase is smaller in this VM's direct samples (96–340 KiB for the three short commands). Retain the original roughly 1 MiB cost as the known, approved tradeoff rather than replacing it with a more favorable machine's measurement. The direct target recheck remains about twice as fast and reduces RSS **120,204 → 101,124 KiB**.

`log-stat-review-recheck.json` retains every timing/RSS sample, randomized execution order, exact argv, fixture paths, CPU details, source-snapshot ids, the two new daily reports, and the paired confidence intervals. Daily `tools.grit_commit` auto-detected checkout tip `81bc746ce`; the artifact explicitly distinguishes that from the two frozen binary source snapshots.

In the original VM, short startup timings moved with their Git controls between daily runs. Thirty randomized interleaved direct samples per tool (after three warmups) rechecked them:

| Command | Git median ms | Before ms | After ms |
| --- | ---: | ---: | ---: |
| log/json-100@git-git | 3.543 | 6.036 | 5.586 |
| log/oneline-100@git-git | 3.766 | 5.961 | 5.627 |
| log/author@git-git | 5.088 | 6.538 | 6.014 |
| show/head@git-git | 2.458 | 4.059 | 3.388 |

These direct timing checks show no slowdown, but direct oneline RSS is **22,244 → 23,252 KiB** and JSON RSS **22,016 → 22,996 KiB**. Most small-command daily RSS rows rise by a similar amount (some approach 2 MiB). The binary is about 183 KiB larger; the extra startup pages come from generic diff instantiations and are recorded as a separate follow-up below. Staged synth-100k daily time also moves 70.49 → 74.09 ms while its Git control moves 98.63 → 101.18 ms; retain the raw figures rather than claiming universal improvement.

Separate CPU-clock perf profiles identify whole-file tokenization and comparison-table construction as dominant baseline work. Heaptrack allocations fall **348,505 → 80,281**, with peak live heap **53.77 → 49.61 MB**. Criterion (20 samples, same-process original counter vs optimized API): **19.74 → 3.17 ms** for a small edit in 100k unchanged lines; **139 → 78 µs** for 1k entirely replaced lines.

## Risks and follow-ups

The maintainer accepted the small-command RSS increase as a deliberate tradeoff for halving target time and reducing target RSS by about 15 MB. Two narrower counting-backend alternatives were measured: they reduced the speed benefit without eliminating that RSS increase, so they were discarded.

Known follow-up: eliminate the extra startup pages introduced by generic diff instantiations. Candidate fixes are a non-generic inner function, dynamic dispatch for the counting sink, or keeping those instantiations out of startup paths. This is separate from #4626's relocation-cost work; do not block this change on it or combine the tasks.

The pre-existing E0277 and reset/fsck flake described above are baseline failures, not failures caused by this change, and remain for the main keeper (#4047). Remaining target memory still exceeds Git, with full blob/session caches prominent in the heap profile. No storage format, wire protocol or rename/merge selection policy was changed.

Committed artifacts: `log-stat-before.json`, `log-stat-after.json`, `log-stat-study.json` and `log-stat-review-recheck.json` beside this report, plus the docs-site benchmark section and updated log usage docs. The study retains all direct timing samples, RSS samples, Criterion estimates, heap summaries and syscall summaries.
