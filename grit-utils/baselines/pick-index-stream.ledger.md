# `grit pick` peak memory: profile and streamed index write

Main: `3c14b849f62053bb6aa30bdcab3a22abea143192`. Measured 2026-10-31 on a four-vCPU Intel
Xeon VM, Linux 6.12, system Git 2.43.0, Rust 1.99.0, Cargo `dist` profile (fat LTO). Daily
JSON: `daily-pick-index-stream-{before,after}.json` (15 to 18 timed runs per tool per
scenario; the `after` report also times the `before` binary as `baseline_grit`).

## Where the memory goes

Peak RSS splits into pages of the `grit` executable and heap. These figures come from
`/proc/<pid>/smaps` read at `exit_group` under gdb, plus heaptrack for the heap peak.
heaptrack reports decimal MB; everything else is MiB.

| `pick/one@synth-10k` | grit | git |
| --- | ---: | ---: |
| Peak RSS (VmHWM) | 21.1 MiB | 9.1 MiB |
| Executable `.text` resident | 11.1 MiB | 2.1 MiB |
| Executable `.text` actually touched (fault-around off) | 3.4 MiB | 1.1 MiB |
| Other executable pages (`.rodata`, `.rela.dyn`, `.data.rel.ro`) | 2.1 MiB | 1.4 MiB |
| Heap (heaptrack peak; git: `[heap]` at exit) | 4.24 MB | 2.9 MiB |

At 10k, more than half of the peak is code. `grit` touches 3.4 MiB of functions spread
across a 25 MiB `.text`. The kernel's 64 KiB fault-around then maps 11.1 MiB of it.
`madvise(MADV_RANDOM)` does not disable fault-around on this kernel. A bare
`grit --version` already costs 6 MiB and `grit log -n1` 12 MiB. The touched code is spread
thin: clap derive (0.3 MiB), the CLI dispatch, and about 40 `grit-lib` modules each
contributing 10 to 300 KiB. No pick-specific function dominates.

The 10k heap peak (4.24 MB) is reached during the tree merge, not in the index write:

| Live at the 10k peak | Size |
| --- | ---: |
| Parsed index entries (10,000 × `IndexEntry`) | 1.12 MB |
| Merge side maps (`build_sparse_maps`, 3 × `BTreeMap<Vec<u8>, IndexEntry>` for 2,000 touched paths) | 1.5 MB |
| Merged result index and its sort scratch | 0.45 MB |
| Tree cache, per-entry path buffers, inflate state | about 0.6 MB |

At 100k the heap dominates. Peak heap was 25.6 MB: 11.2 MB of parsed entries plus a
10.4 MB `Vec` holding the complete serialized index, built before hashing and writing it.
Git streams its index through a small hashing buffer instead.

## Change

`Index` writes serialize straight into the lock file through a 128 KiB buffer, updating the
checksum as each chunk drains (`IndexBodyOut` in `grit-lib/src/index/core.rs`). IEOT block
offsets and the EOIE extension offset are absolute body positions tracked across drains, so
the file is byte-identical to the old in-memory serialization. Unit tests compare the two
for index v2 and v4, with and without `skip_hash`, and through a held `IndexLock`. The
`index_eoie_ieot`, split-index, index-version and add-write git-compat suites (63 tests,
including `git fsck` and `git update-index` on a 20,000-entry IEOT/EOIE index written by
grit) pass. This covers every index write (`add`, `commit`, `switch`, `merge`, `pick`,
`reset`, …), not only pick.

## Results

Peak RSS and medians come from the daily reports; peak heap comes from heaptrack.

| Scenario | Before | After | Git |
| --- | ---: | ---: | ---: |
| `pick/one@synth-100k` peak RSS | 45.9 MiB | 38.4 MiB | 37.1 MiB |
| `pick/one@synth-100k` median | 148.9 ms | 145.5 ms | 192.5 ms |
| `pick/one@synth-100k` peak heap (heaptrack) | 25.6 MB | 17.0 MB | — |
| `pick/one@synth-10k` peak RSS | 21.7 MiB | 21.6 MiB | 9.0 MiB |
| `pick/one@synth-10k` median | 54.8 ms | 55.5 ms (old binary in the same run: 55.2 ms) | 115.4 ms |
| `pick/one@synth-10k` peak heap (heaptrack) | 4.24 MB | 4.24 MB | — |

Time is unchanged within noise at both sizes. At 10k, peak RSS is unchanged because neither
the peak heap nor the code pages depend on the index write.

## Remaining gap at 10k: code layout

As an experiment, not committed, the functions touched by `grit pick` were linked first
through LLD's `--symbol-ordering-file` (4,795 symbols, from the page trace above). Both
binaries include the streamed index write, and peak RSS is the median of `/usr/bin/time`
runs:

| Ordered link experiment | Plain `dist` | Hot functions first |
| --- | ---: | ---: |
| `.text` resident, `pick/one@synth-10k` | 11.1 MiB | 5.0 MiB |
| Peak RSS, `pick/one@synth-10k` | 20.7 MiB | 15.1 MiB |
| Peak RSS, `pick/one@synth-100k` | 37.8 MiB | 32.7 MiB |
| `grit status` peak RSS (same fixture) | 16.7 MiB | 13.7 MiB |

Wall time did not change. A hand-written ordering file breaks whenever symbol hashes
change, so the durable form of this is a profile-guided layout in the release build (PGO,
or an ordering file generated from a training run of the daily scenarios). That is a
build-pipeline change affecting every command, and it is the largest remaining lever on
small-repo peak RSS. Shrinking the merge side maps (about 1 MiB at 10k) would mean changing
the value type that `merge_trees.rs` consumes, which other in-flight work is changing.
