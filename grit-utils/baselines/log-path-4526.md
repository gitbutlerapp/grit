**Task #4526: path-limited history**

Ready for review on mesh `grit-perf-4526-r2:grit/factory/log-path-speed`. Code commit: `5d70e8b7874fb32b190d2ea16b263626515ab21f`; comparison base: `818de8006e5b951099934d552812373b51f5ebce`. The base was pinned when the branch was rebased; later upstream changes are outside this comparison.

`log/path@git-git` improves **6.98×**, from **3229.01 ms to 462.62 ms**, against **370.22 ms** for system Git. Target peak RSS falls **12.3%**. Bounded path log improves about 3×. Every other measured median is unchanged within 1% or faster; the largest small-row RSS increase is 104 KiB, preserved in the table.

Dense path history parsed complete trees into owned entry vectors, repeated that work when resolving file leaves, and kept comparing merge parents after finding the parent it would follow. The library now resolves requested entries in reusable raw-tree buffers, interns component keys, retains reusable packed delta bases through both pack shortcuts and the files primary, and stops at the first TREESAME parent. Existing commit-object caching is preserved for filters, with a regression test checking that filtering does not reopen the commit.

The added compatibility matrix also exposed a redundant sparse-history filtering pass that discarded selected merges; removing that pass makes sparse results match Git. Tests cover packed file/directory transitions, merges matching only their second parent, missing paths, sparse/show-pulls/first-parent selection, SHA-1/SHA-256 and commit-graph Bloom filters, plus overlays, alternates and custom object stores. The library guide documents these semantics; the benchmark page and registered JSON baseline contain the measurements.

Measurements used release builds, stock `/usr/bin/git` 2.55.0, Rust 1.99.0, a four-vCPU AMD EPYC 7763 VM and pinned git.git `v2.47.0` (`777489f9e09c8d0dd6b12f9d90de6376330577a2`). `grit-bench daily --only ...` prepared and exercised every registered scenario. Final samples use its `measure-direct-run` helper, one warmup per binary, then alternating git/before/after and after/before/git: 10 samples per tool, 30 for short commands, with no compilation or tests running. Values are median wall milliseconds and maximum peak RSS in KiB. GNU `/usr/bin/time -v` independently measured the target at 3.23 s / 310,844 KiB before, 0.45 s / 273,504 KiB after, and 0.38 s / 252,764 KiB for Git.

| Scenario | Git ms | Grit before ms | Grit after ms | RSS before KiB | RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| `cat/blob@git-git` | 1.40 | 2.58 | 2.56 | 13,460 | 13,500 |
| `diff/worktree@synth-10k` | 14.47 | 14.67 | 14.39 | 19,072 | 18,812 |
| `log/all@git-git` | 51.56 | 71.72 | 72.16 | 35,504 | 35,112 |
| `log/author-n50@deep-history` | 1.64 | 3.69 | 3.67 | 17,252 | 17,356 |
| `log/author-n50@deep-history-cg` | 1.99 | 3.83 | 3.80 | 19,820 | 19,472 |
| `log/author@git-git` | 2.55 | 4.82 | 4.83 | 21,328 | 21,316 |
| `log/json-100@git-git` | 3.25 | 5.24 | 5.20 | 23,056 | 22,664 |
| `log/json-filter@git-git` | 2.40 | 5.41 | 5.39 | 21,860 | 21,688 |
| `log/limit-path@git-git` | 25.02 | 62.81 | 20.58 | 40,668 | 36,280 |
| `log/oneline-100@git-git` | 3.40 | 4.66 | 4.66 | 22,420 | 22,036 |
| `log/oneline-100@synth-100k` | 2.47 | 3.34 | 3.33 | 18,976 | 18,640 |
| `log/path@git-git` | 370.22 | 3229.01 | 462.62 | 312,212 | 273,720 |
| `log/stat-n50@git-git` | 102.96 | 391.32 | 386.58 | 122,236 | 117,224 |
| `merge-base/branches@git-git` | 25.00 | 4.66 | 4.62 | 22,552 | 22,520 |
| `revs/count@git-git` | 546.55 | 776.43 | 778.86 | 127,428 | 127,156 |
| `show/head@git-git` | 2.08 | 3.12 | 3.12 | 16,756 | 16,636 |
| `revs/count-path-head@git-git` | 453.37 | 2512.87 | 513.56 | 311,044 | 272,028 |


`perf record -e cpu-clock:u -F 499 --call-graph dwarf,16384` attributes 19.5% of baseline CPU to tree-entry parsing, plus 8.1% to object-id decoding and substantial allocation/freeing overhead. Heaptrack allocation calls fall **25,201,132 → 467,713** (98.1% fewer), with peak heap **152.83 → 121.42 MB**. `strace -c` confirms syscalls are a small fraction of elapsed time. Cache-size settings were not changed.

Validation:

- Exact-base comparable suites: **2,934 passed, 0 failed, 7 ignored** before; **2,942 passed, 0 failed, 7 ignored** after. Eight new library tests were added. All 16 common test targets fully pass before and after (`files_fully_passing_delta = 0`).
- Final expanded run: **3,105 passed, 0 failed, 7 ignored** across 24 targets, including 163 additional existing CLI rendering/filter/snapshot tests. This runs `cargo test -p grit-lib -p grit-cli --lib --bins`, 14 storage/history integration targets and eight CLI integration targets.
- Test processes set `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=gc.auto`, `GIT_CONFIG_VALUE_0=0` on both sides. Unmodified default runs exposed an existing background Git auto-GC / `fsck` fixture race on both baseline and branch; the same 31 reset tests pass with background auto-GC disabled. No assertions or ignores were weakened.
- `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, baseline schema validation and all seven site benchmark tests pass.

Known upstream blockers and follow-ups, left outside this change:

- The required full `cargo test -p grit-lib -p grit-cli` attempts before/after fail to compile the existing `history_injected_ref_store` test (`Signature: From<&str>`). `cargo test -p grit-lib` also finds stale `ServeOptions` / upload-pack test initializers. The full package gate is therefore **not green** despite the passing scoped runs above.
- Rustdoc fails on three existing links: `CommitRequest::index_only`, `Error::Cancelled`, and the public link to private `relative_deepen_depth_for_request`. `npm run check` is consequently blocked by missing generated rustdoc; benchmark-specific site tests pass. These unrelated API/docs files were left untouched.
- Daily `revs/count-path@git-git` names `7e34d81`, absent from the pinned fixture. The report includes a clearly named `revs/count-path-head@git-git` replacement at pinned HEAD instead.

The main risk is shared packed-object lookup/cache behavior; existing non-caching read defaults are preserved and storage interoperability tests cover the affected routes. Full integration still needs the current-main gate after the separate integrator rebases the branch.
