# Branch JSON listing: shared environment snapshots (task 4643)

Main: `f23547129b04beca9bb4f38646eec82bf9f186f2`. Implementation:
`4741577c8247a5ec5008416c6528ea49c6d1271a`.
Measured 2026-10-26 on the same AMD EPYC 7763 VM (four vCPUs), Linux, system
Git 2.55.0, Rust 1.99.0, Cargo `release` profile (no fat LTO).

Repository discovery and config loading repeatedly clone `Environment`, whose captured
variable map previously deep-copied all keys and values. The immutable map is now shared
with `Arc`; typed fields still clone independently, and empty captures allocate no map.

## Daily matrix and RSS

Daily wall times are medians of 20 timed runs per executable. RSS is the median of the
maximum resident set from 20 separate `/usr/bin/time -v` runs per executable, with tool
order alternating. The committed daily JSON also retains the runner's own maximum RSS
across its samples; that is a different statistic from the median RSS below.

| Scenario | Git ms | Grit before ms | Grit after ms | Peak RSS before KiB | Peak RSS after KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| branch/create@many-refs | 2.319 | 3.864 | 3.731 | 13,548 | 13,272 |
| branch/delete@many-refs | 2.539 | 10.184 | 9.918 | 14,682 | 14,434 |
| branch/list-json@many-refs | 2.344 | 3.448 | 3.326 | 14,126 | 13,890 |
| branch/list@many-refs | 2.089 | 3.317 | 3.130 | 12,292 | 12,030 |
| branch/list@many-refs-loose | 2.017 | 3.120 | 3.059 | 12,350 | 12,032 |
| branch/merged-ref@many-refs | 1.358 | 3.031 | 2.934 | 10,314 | 10,252 |
| branch/merged@many-refs | 2.463 | 3.645 | 3.689 | 13,992 | 13,690 |
| branch/rename@many-refs | 2.528 | 8.940 | 9.029 | 12,386 | 12,356 |
| config/get@small | 1.858 | 2.970 | 2.954 | 10,820 | 10,870 |
| status/clean@synth-10k | 17.361 | 19.064 | 18.913 | 16,980 | 17,080 |
| tag/list@many-refs | 5.269 | 6.452 | 6.519 | 12,574 | 12,832 |

Both versions of `grit-bench` were compiled from their corresponding library source.
This matters for `branch/rename` and `branch/merged-ref`: these rows invoke library helpers
inside `grit-bench`, so changing only `--grit` does not compare the helper implementations.
No measured wall-time increase exceeds the spread in the 20-run samples (the small positive
shifts are merged +1.2%, rename +1.0%, tag +1.0%).

Control RSS needs care: tag listing increased 258 KiB (+2.1%) in the repeated sample
(354 KiB / +2.8% initially), clean status increased 100 KiB (+0.6%), and config get
increased 50 KiB (+0.5%). Tag heaptrack peak live heap decreased from about 1.24 MB
to 1.22 MB and allocations decreased 22,030 → 20,928. A delayed-exit residency sample also showed higher retained anonymous pages, so this
is not attributed solely to code-page residency. These process-RSS shifts conflict with the
strict zero-regression requirement and need a maintainer decision before this is called done.
A compact boxed-string variant was measured and discarded because it did not resolve the
control RSS increase. The retained implementation is the smaller, fully tested change above.

## Direct headline check

`hyperfine --shell=none --warmup 20 --runs 300` in both command orders, for 600 runs per
executable, on the same many-refs working copy:

| Command | Median ms | Mean ms |
| --- | ---: | ---: |
| `git branch -vv` | 1.916 | 1.924 |
| `main/grit branch --json` | 2.779 | 2.797 |
| `work/grit branch --json` | 2.709 | 2.727 |

The cached fixture has 10,000 refs; its working clone lists one local branch by default.
Human, JSON, and Markdown output are compared byte-for-byte on this fixture.

Heaptrack on the headline command: allocation calls 5,552 → 4,447 (19.9% fewer); peak
live heap stayed about 323 KB, set earlier by clap parsing. `perf record -e cpu-clock:u
-F 3999 --call-graph dwarf` sampled 500 invocations; malloc/free and environment map clones
were visible. `strace -c` confirms the work is process/config setup, not bulk object reads.

## Library benchmark

`cargo bench -p grit-lib --bench environment -- --warm-up-time 1 --measurement-time 2
--sample-size 30`, with the main baseline saved before measuring the change:

| Captured variables | Before | After |
| ---: | ---: | ---: |
| 0 | 154.13 ns | 141.89 ns |
| 64 | 5.155 µs | 195.91 ns |
| 256 | 20.358 µs | 193.61 ns |
| 1,024 | 80.532 µs | 183.90 ns |

## Validation

- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace -- -D warnings`: passed.
- `cargo test -p grit-lib --lib`: before 2,580 passed / 3 ignored; final default parallel
  run 2,582 passed / 3 ignored. Earlier intermediate runs hit three object-count/reset
  fixture failures; isolated repetitions and the final default parallel run pass unchanged.
- Public environment/repository/branch integration suites: 12 passed. The packed-ref clone
  compatibility case also passed. The branch mutation compatibility suite passed 12 tests,
  including grit-written refs/objects checked with system Git and `git fsck --strict`.
- CLI branch suites: 24 passed. All three output formats match the main binary byte-for-byte.
- Required full `cargo test -p grit-lib -p grit-cli` is blocked on existing main compilation
  errors: `serve_upload_pack_inprocess` uses an obsolete `ServeOptions` shape, and
  `history_injected_ref_store` constructs `Signature` with `TS.into()`. The latter was
  rechecked on the final implementation with the same compilation failure.
- An isolated broader CLI run passed 598 tests before stopping at
  `json_output::bad_filter_and_missing_revision_errors_are_clean`; the untouched main binary
  returns the same no-commits error for that empty-repository/filter reproduction.
- The full packed-ref compatibility file has another fixture case that system Git 2.55.0
  rejects while packing a tag object under `refs/heads`, before grit runs. It is unchanged.
- Rustdoc is blocked by main's three invalid links (private upload-pack helper,
  `CommitRequest::index_only`, and `Error::Cancelled`); strict site checking therefore lacks
  the required rustdoc output. Site tests: 36 passed, 1 failed, 8 skipped, plus the rustdoc
  suite initialization failure; both failures follow missing rustdoc. No test assertions or
  lint levels were changed.

## Reproduction

Build both revisions with `cargo build --release -p grit-cli -p grit-utils`, preserving both
executables for each revision under distinct directories. Run each matching helper:

```bash
/path/to/grit-bench --grit /path/to/grit daily --only \
'branch/list-json@many-refs,branch/list@many-refs,branch/list@many-refs-loose,branch/create@many-refs,branch/delete@many-refs,branch/rename@many-refs,branch/merged@many-refs,branch/merged-ref@many-refs,config/get@small,status/clean@synth-10k,tag/list@many-refs' \
--output /tmp/branch-results.json
```

Use the same working copy for the direct headline comparisons and discard stdout in RSS
runs. Mutating RSS samples remove both benchmark branch names first, and recreate the
source branch for delete/rename. No source files in branch inventory, ref scanning, or linker
startup were changed; the branch is independent of the corresponding work in flight.

## Repeated RSS samples (KiB)

```json
{"branch/create@many-refs":{"git":[5500,5872,5692,5564,5740,5928,5836,5728,5564,5808,5776,5796,5572,5924,5804,5792,5660,5792,5684,5484],"before":[13732,13544,13596,13380,13728,13848,13296,13412,13488,13224,13608,13552,13292,13656,13412,13412,13800,13408,13656,13792],"after":[13348,13336,13248,13180,13384,13176,13240,13120,13084,13184,13296,13296,13344,13308,13224,13404,13180,13180,13404,13512]},"branch/delete@many-refs":{"git":[5408,5668,5624,5316,5740,5272,5408,5424,5452,5676,5456,5336,5676,5560,5576,5456,5728,5440,5628,5516],"before":[14524,14600,14680,14760,14700,14828,14468,14740,14788,14596,14752,14700,14832,14356,14684,14516,14508,14356,14728,14596],"after":[14304,14544,14152,14564,14516,14216,14292,14384,14520,14292,14416,14224,14224,14580,14456,14664,14584,14288,14516,14452]},"branch/list-json@many-refs":{"git":[5596,5740,5620,5732,5560,5860,5864,5660,5632,5628,5556,5716,5848,5716,5608,5732,5500,5688,5692,5796],"before":[14172,14380,14496,13996,13896,14124,14208,14060,14504,14060,14188,13992,13996,14128,13996,14176,14112,14228,13996,14164],"after":[13712,13804,13668,13944,13952,13884,13956,13924,13348,13784,13944,13720,13976,13836,13404,13952,13924,13964,13592,13896]},"branch/list@many-refs":{"git":[5056,4812,5036,4732,4968,4892,4796,4860,4904,4864,4796,4792,4764,4864,4844,4732,4720,4900,4748,5092],"before":[12164,12292,12532,12160,12164,12476,12292,12140,12484,12396,12212,12228,12548,12412,12292,12544,12292,12160,12548,12548],"after":[11672,12164,12120,12116,12056,12128,11952,12180,12100,11972,11704,12056,11940,11696,12052,12008,12176,11940,11692,11936]},"branch/list@many-refs-loose":{"git":[5092,4732,4968,4920,4968,4864,4844,4968,4844,4892,4856,4852,4716,4820,4844,4780,4920,4756,4968,4728],"before":[12228,12224,12164,12540,12548,12420,12164,12152,12408,12548,12228,12520,12532,12416,12228,12292,12228,12164,12548,12416],"after":[11960,12068,11672,12008,12072,12232,11972,12216,11700,12056,12360,12076,11672,11972,11672,11692,12092,12232,12352,11700]},"branch/merged-ref@many-refs":{"git":[5472,5420,5312,5548,5412,5248,5552,5480,5296,5548,5468,5612,5312,5312,5484,5408,5568,5552,5616,5412],"before":[10384,10396,10584,10284,10076,10156,10280,10320,10100,10736,10276,10308,10220,10212,10444,10444,10384,10400,10572,10076],"after":[10140,10272,10192,10232,10028,10144,10284,10028,10112,10540,10596,10560,10056,10180,10564,10148,10620,10480,10272,10480]},"branch/merged@many-refs":{"git":[5796,5488,5508,5616,5744,5608,5728,5700,5628,5908,5680,5376,5800,5496,5700,5804,5816,5432,5600,5708],"before":[14104,13992,14108,13848,13864,13856,14108,14120,14312,14080,13956,14120,13992,14368,13948,13824,13988,14104,13896,13988],"after":[13884,13848,13852,13388,13520,13916,13692,13556,13868,13568,13552,13756,13556,13660,13928,13692,13756,13152,13688,13576]},"branch/rename@many-refs":{"git":[4996,4988,5036,4988,4844,4728,4932,5120,5124,4852,4964,5072,5148,4996,5104,4976,5112,4860,4804,5104],"before":[12424,12484,12484,12412,12272,12296,12312,12152,12360,12444,12188,12196,12556,12492,12480,12328,12236,12220,12432,12440],"after":[12208,12416,12484,12220,12352,12240,12448,12396,12184,12228,12408,12384,12412,12184,12320,12232,12448,12456,12188,12360]},"config/get@small":{"git":[4156,4028,4064,4016,4224,4068,4076,3972,3920,4004,3920,4216,4096,4000,4156,4144,3972,3920,3908,3920],"before":[10948,10820,10820,10648,10564,10436,10948,10692,10820,10884,10564,10820,10940,10948,10820,10692,10820,10820,10436,10948],"after":[10980,10488,10976,10500,10972,10624,11056,11044,10768,10764,10768,11048,10624,10972,10492,10972,10768,10764,11048,10972]},"status/clean@synth-10k":{"git":[7120,7188,7476,7120,7176,7180,7356,7312,7200,7184,7184,7316,7264,7312,7184,7096,7204,7124,7396,7376],"before":[16924,16932,16932,16872,17104,16940,16916,17108,17252,16976,16908,16972,17032,17064,16996,17260,16984,17192,16936,17044],"after":[16956,17004,17320,16840,16944,17068,17104,17020,17312,17092,17340,17244,17128,17068,17020,17204,16956,17016,17096,17436]},"tag/list@many-refs":{"git":[6076,5864,5996,5976,6036,5860,5856,6056,6036,6056,5864,6072,5796,5864,6076,5988,6072,5928,5964,5920],"before":[12392,12676,12548,12600,12464,12336,12448,12676,12696,12804,12280,12572,12536,12816,12604,12460,12200,12596,12576,12728],"after":[12756,12716,12864,12756,12700,12904,12488,12840,12924,12968,12568,12884,12864,12764,12824,12752,12580,13000,12860,12960]}}
```
