# Task 4567: checkout cancellation batches

The six target scenarios improve 13–25% against the pinned main baseline, with cancellation and bounded preparation retained. All eight added tests pass; the remaining test failures and API compile blockers also occur on the baseline.

All six reported regressions reproduced against system Git 2.55.0. The first bad commit is `63ea7f3efbbd5c901c5cf0655a654d2e1cd4d57b`; its parent is `7cfa6552644f5101155f761600ae23a8e5f9f186`. A read-only, profile-directed bisect also built and measured `42e7d3bad` and its parent; large-blob streaming was not the first bad change. Historical endpoint and direct-parent probe samples are retained in the audit JSON.

The cancellation change flushed eight files at a time and tested the parallel threshold on each flush, so checkouts that qualified as a whole ran serially and repeatedly resolved worker policy. This branch chooses the policy once, grows cancellation batches from eight to 2,048 files, and reuses small loose reads during bulk checkout while bounding prepared bytes, streamed writes and large cached-object copies. Direct writes and the one-pass loose-object walk bypass the new cache bookkeeping, preserving their prior read path.

The feature remains: early cancellation still stops quickly, later work is bounded, and streamed files are limited to eight per prepared flush. Ordered preparation keeps at most two blobs per worker ahead of the consumer, in addition to the 1 MiB prepared-content flush threshold. Existing worker-count and threshold overrides remain respected. No CLI/API interface, output schema, or Git-format behavior changes.


Historical endpoints and direct-parent confirmation use the same balanced order, one warmup and 12 timed runs/tool. All six regressions reproduce; none is dropped as machine noise. The Git column below is the endpoint-session control; the separate first-bad Git control is in the audit.

| Scenario | Git ms | Oct 24 ms | Oct 25 ms | Parent of first bad ms | First bad ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `worktree/add@synth-10k` | 327.5 | 320.0 | 427.7 | 326.5 | 430.7 |
| `merge/clean@synth-10k` | 170.4 | 143.2 | 195.6 | 146.3 | 209.8 |
| `pick/one@synth-10k` | 182.8 | 150.8 | 192.4 | 154.6 | 202.8 |
| `revert/one@synth-10k` | 186.6 | 145.6 | 203.1 | 166.9 | 210.9 |
| `pick/one@synth-100k` | 284.7 | 298.0 | 345.1 | 288.9 | 354.3 |
| `clone/checkout@synth-100k` | 7155.8 | 5816.7 | 7386.5 | 5819.8 | 7395.0 |

## Measurement

Both release endpoints, the first-bad probes, and the branch were measured on this VM against `/usr/bin/git`. The final comparison pins the origin/main snapshot `12b52a55c` and production code `caf6f64fa`; origin/main subsequently advanced, so later upstream changes are excluded. Builds use `cargo build --release -p grit-cli -p grit-utils`. Machine: AMD EPYC 7763, four available logical CPUs, 15.6 GiB RAM, ext4, Rust 1.99.0, Git 2.55.0.

Final local scenarios use one warmup and 12 timed runs per executable. The repeating order is GBA, GAB, ABG, BGA, BAG, AGB (G=Git, B=before, A=after), with warmups GAB. Every executable follows each other executable equally, including trial boundaries. Identical-binary controls exposed predecessor bias in preliminary rotations; those values are excluded from the final comparison. Additional scenarios use the stock daily A/B runner with ten timed runs/tool and separate before/after fixtures and HTTP server lifetimes. No Rust builds or profiling ran during the final timing matrix. Peak RSS is the maximum child-process high-water mark, corroborated with `/usr/bin/time -v`.

Fixture preparations use the daily harness. For repeated merge/pick trials, the topic ref is restored from fixture metadata and starting HEAD is checked, avoiding a no-op merge after a prior revert. Warmups and last measured iterations verify matching Git trees/indexes, clean tracked diffs and `git fsck --strict`. The raw commands, samples, preparation corrections, binary hashes and profiles are in `checkout-chunks-4567-audit.json`.

| Scenario | Git ms | Grit before ms | Grit after ms | Peak RSS KiB before / after |
| --- | ---: | ---: | ---: | ---: |
| `switch/branch@synth-10k` | 46.9 | 33.4 | 33.6 | 22,824 / 22,736 |
| `restore/paths@synth-10k` | 21.2 | 21.5 | 21.7 | 22,572 / 22,652 |
| `reset/hard@synth-10k` | 42.6 | 45.6 | 45.0 | 25,196 / 25,208 |
| `worktree/add@synth-10k` | 325.3 | 426.0 | 320.7 | 26,164 / 27,124 |
| `merge/clean@synth-10k` | 169.0 | 184.1 | 145.6 | 28,148 / 27,400 |
| `pick/one@synth-10k` | 182.5 | 185.1 | 161.8 | 27,620 / 27,992 |
| `switch/branch@synth-100k` | 241.6 | 107.3 | 105.5 | 54,692 / 54,816 |
| `switch/delete-heavy@wide-48k` | 227.6 | 207.3 | 196.3 | 35,376 / 35,784 |
| `switch/large-blob@big-asset` | 843.2 | 478.6 | 474.2 | 18,648 / 18,508 |
| `merge/clean@synth-100k` | 458.2 | 274.7 | 234.8 | 59,492 / 60,240 |
| `revert/one@synth-10k` | 182.6 | 194.7 | 158.3 | 28,176 / 28,240 |
| `pick/one@synth-100k` | 285.5 | 347.1 | 296.4 | 58,140 / 58,496 |
| `clone/checkout@synth-100k` | 7077.6 | 5933.7 | 4473.5 | 79,072 / 81,128 |
| `merge/content-both-sides@wide-20k` | 2113.5 | 3020.0 | 2679.7 | 112,440 / 114,052 |
| `clone/local@synth-10k` | 1052.4 | 1364.6 | 1264.7 | 31,172 / 31,804 |
| `clone/http@synth-10k` | 1139.6 | 1415.9 | 1308.9 | 31,964 / 32,260 |
| `clone/bare@synth-10k` | 711.4 | 795.3 | 796.8 | 24,300 / 24,680 |
| `clone/shallow-depth1@synth-10k` | 412.5 | 504.4 | 398.2 | 27,804 / 28,176 |
| `clone/shallow-depth1-http@synth-10k` | 422.6 | 514.6 | 408.7 | 27,600 / 28,176 |
| `worktree/remove@synth-10k` | 204.1 | 174.1 | 174.6 | 13,620 / 13,496 |
| `worktree/list@synth-10k` | 2.9 | 3.7 | 3.6 | 12,648 / 12,752 |
| `reset/hard-delete-heavy@wide-48k` | 380.8 | 421.3 | 410.3 | 59,296 / 62,828 |
| `reset/mixed@synth-10k` | 15.7 | 18.9 | 18.7 | 20,208 / 20,480 |
| `clone/full@git-git` | 20627.0 | 10821.3 | 10643.9 | 185,648 / 184,988 |
| `clone/shallow@git-git` | 2147.0 | 2312.6 | 2241.0 | 73,308 / 72,148 |
| `clone/blobless@git-git` | 12120.1 | 9740.9 | 9656.4 | 93,232 / 88,988 |

Additional A/B scenarios have separate Git controls before and after; the table shows the after control, with both retained in the JSONs. `clone/local` is smart HTTP despite its historical scenario name. Differences below about 1 ms in small cases are comparable to the identical-binary control spread and are not claimed as speedups.

On `pick/one@synth-10k`, final syscall profiles reduce `openat` calls from 5,099 to 4,103, reads from 5,592 to 4,098, and CPU-affinity queries from 258 to 65. Heaptrack reports **312,599→287,497 allocation calls** (8.0% fewer) and **85,289→78,334 temporary allocations**; reported peak heap stays **7.34M**. Git uses 86,425 allocations and 7.14M reported peak heap. Instrumented time and RSS are excluded from the timing table.

HTTP controls were re-run with the required helper built. The shallow Git-history fixture used a relative destination from its fetch-client cwd but verified an absolute sibling directory; the external driver normalizes that cwd to the destination parent and keeps all pre/post checks. No benchmark or production assertion is weakened.

## Validation

Eight active tests were added: six unit tests for policy/cache/chunk behavior, a Linux memory integration test covering 256 distinct 512 KiB blobs with serial and parallel preparation plus an already-cached 64 MiB blob, and a large-stream cancellation integration test. The memory test round-trips through system Git (`write-tree`, status, fsck). No existing tests were removed or weakened.

The cached-object guard was demonstrated red: a bounded lookup copied the full cached blob and raised RSS by 65,176 KiB. Checking size under the cache lock before cloning reduces that growth to 172 KiB. Distinct medium-blob growth is 3,676 KiB serial and 9,592 KiB parallel, below the 16 MiB guard.

| Runnable validation | Before passed / failed | After passed / failed |
| --- | ---: | ---: |
| Library unit tests | 2,579 / 3 | 2,587 / 1 |
| CLI tests | 598 / 1 | 598 / 1 |
| Checkout/related compatibility targets | 34 / 1 | 36 / 1 |
| Total | 3,211 / 5 | 3,221 / 3 |

Three unit tests are ignored in both runs. Fully passing selected test targets increase by one (the added memory integration file). These counts describe runnable groups, not a green full workspace gate.

`cargo fmt` and `cargo clippy` pass. Required full-test commands were attempted before and after. The final rebuild exhausted disk space while linking accumulated debug executables; after cleaning only build artifacts, final verification was rebuilt with debug symbols disabled and assertions unchanged. Fresh retries of both full commands now fail on the same existing API blockers seen before: old Signature conversions in `history_injected_ref_store` and missing server-options arguments in `serve_upload_pack_inprocess` (14 compile errors). Separate compile probes confirm both fixtures.

The final library unit run has 2,587 passes and one reset fsck failure also seen on the baseline: system Git tries to mmap a loose object while it disappears. Its stat/probe assertions pass. CLI has the same bad-filter JSON failure as baseline, and compatibility tests retain the same backward-reset cancellation expectation failure. All eight added active tests pass. No test or assertion was disabled or weakened, and no green full-suite gate is claimed.

Rustdoc fails three existing links: a public-to-private serve helper link, removed `CommitRequest::index_only`, and unresolved `Error::Cancelled`. Site validation consequently stops at missing local rustdoc. Documentation/data validation was attempted again after adding these baselines. Both new 26-row reports load successfully through the site's own daily-report loader; sample counts, ratios and RSS fields also pass independent validation.

## Risks and follow-ups

- Later cancellation can complete up to 2,048 small-file jobs between chunk boundaries, with byte bounds limiting larger bodies and streamed writes capped at eight. Early chunks remain eight, then double. Worker threads do not call progress callbacks.
- The session cache reuses only small bounded loose reads (at most 64 KiB each); public ODB reads can already cache larger objects, so bounded hits inspect size before copying.
- Cache reuse is scoped to bulk checkout. Direct writes and one-pass object traversal retain the original bounded read path, keeping its final timing within the identical-binary control spread.
- Populated checkout's existing ChunkProgressSink does not forward `ProgressSink::should_cancel`; explicit Cancel flags work and are covered. This is outside the batching fix.
- The 100k-file clone uses about 2 MiB more peak RSS for concurrent work in this sample; allocations and prepared bodies remain bounded. The 100k-file pick is still about 1.04× Git despite its 15% improvement.
- Repair the pre-existing full-test and rustdoc gates separately; origin/main has moved since this pinned comparison.
- Daily preparation leaves predecessor-dependent state; canonicalize it or balance trial boundaries in the runner. The explicit RunPolicy here also avoids the daily entrypoint's fixed eight-run default ignoring the global minimum-run setting.

Work is on `grit-perf-4567-r3/grit`, branch `factory/checkout-parallel-chunks`. No push, pull request or integration was performed.

## Cases without measurements

- `reset/keep@synth-10k` (before): No executable daily plan is registered for this matrix ID.
- `reset/keep@synth-10k` (after): No executable daily plan is registered for this matrix ID.
- `restore/hunks@large-file` (before): No executable daily plan is registered for this matrix ID.
- `restore/hunks@large-file` (after): No executable daily plan is registered for this matrix ID.

No timing is invented for failed cases. Successful paired scenarios are shown above.

Failed-case details are retained in the audit JSON.

Artifacts: `daily-checkout-chunks-before.json`, `daily-checkout-chunks-after.json`, and `checkout-chunks-4567-audit.json` in `grit-utils/baselines`.
