# Grit performance: where it struggles and how to fix it (lib-concentrated)

_Generated from `bench/run-everyday.sh` (git vs grit across repo scales) + profiling, 2026-06._

## TL;DR

Grit is competitive (or faster) than C git on process-startup- and batch-dominated
commands, but it has **pervasive super-linear scaling on the number of files**, caused
by **re-loading `.gitattributes` and the config cascade inside per-file loops** instead
of once. The single highest-leverage fix is to **cache attribute + config loading in
`grit-lib`**; that alone collapses the worst blowups (e.g. `grep` on 10k files: **50.5 s
→ expected < 0.1 s**) and helps every command that touches the work tree.

## Method

`bench/run.sh` and `bench/run-everyday.sh` source `bench/env.sh` so **git and grit-git
see the same hermetic config**: a temp `HOME`, `GIT_CONFIG_NOSYSTEM=1`, and a generated
global config (`user.name`/`user.email`, `commit.gpgsign=false`, `init.defaultBranch=main`).
Inherited `GIT_*` variables are stripped so a runner's signing program cannot skew grit
only. **Earlier commit ratios (e.g. 6–21× at M/L) measured grit waiting on an external
GPG/SSH signer** while git had `-c commit.gpgsign=false`; those numbers are not
comparable.

`bench/run-everyday.sh` benchmarks the commands people run daily/weekly (status, add,
commit, log, diff, checkout, restore, branch, show, grep, blame, shortlog, merge, rebase,
cherry-pick, reset, stash, ls-files, write-tree, local clone) with `hyperfine`, across
four repo scales:

| Scale | Files | Commits | Stresses |
|---|---|---|---|
| **S** | 100 | 100 | baseline / fixed overhead |
| **M** | 1,000 | 500 | (the old single-scale bench) |
| **L** | 10,000 | 1,000 | working-tree / index / per-file cost |
| **H** | 500 | 4,000 | history walks |

## Findings — grit/git wall-clock ratio (>1 = grit slower)

| command | S | M | L | H |
|---|--:|--:|--:|--:|
| grep | – | **1031×*** | **44×*** | 44× |
| stash | 3.3× | 17× | **1030×** | 3.7× |
| merge | 28× | 1.3× | **945×** | 52× |
| rebase | 42× | 11× | **943×** | 341× |
| reset | 2.4× | 8× | **94×** | 1.7× |
| add | 1.7× | 7.2× | **61×** | 3.7× |
| cherry-pick | 1.8× | 7.8× | 44× | 2.3× |
| commit (`commit@`, staged, `-q`) | 2.5× | 2.1× | 3.6× | 3.5× |
| checkout | 1.5× | 0.5× | 17.5× | 3.1× |
| log --stat | – | 13.6× | 15.8× | 15.4× |
| log -p | – | 6.8× | 13.8× | 6.6× |
| ls-files | 1.0× | 2.2× | **8.8×** | 1.6× |
| diff --staged | – | 2.5× | 6.2× | 2.4× |
| status | 0.8× | 1.7× | 3.7× | 1.6× |
| diff | – | 2.2× | 3.0× | 2.4× |
| log --oneline | – | 1.4× | 2.2× | 5.4× |
| restore / shortlog / show / branch | – | – | **0.6–1.0×** | 0.7–1.1× |

\* `grep`'s hyperfine ratio is warm-cache-skewed; the true cold cost is far worse — see
the profile below. The pattern is unmistakable: **ratios grow with file count** (the
old M-only bench hid this). The combo commands (stash/merge/rebase/reset/cherry-pick)
explode at L because they drive checkout + diff + index over every file.

## Root cause (profiled)

`grit grep -n 'line two'` on the **L (10k-file)** repo:

```
git  grep : 0.00 s
grit grep : 50.49 s      ← >5000× slower
```

`sample` of that run is dominated by:

```
grit_lib::attributes::load_gitattributes_for_diff
  └ load_gitattributes_stack
      └ walk_dirs                 ← directory walk + .gitattributes parse, PER FILE
```

`grit/src/commands/grep.rs` calls, **inside the loop over every tracked file**:
- `funcname_matcher_for_path()` → `ConfigSet::load(git_dir)` **and** `load_gitattributes_for_diff(repo)`
- `blob_as_grep_text()` → `ConfigSet::load(git_dir)`

`grit_lib::attributes::load_gitattributes_for_diff` (grit-lib/src/attributes.rs) has **no
cache** — it re-resolves the attr tree-ish and rebuilds the whole attribute stack every
call. `ConfigSet::load` re-parses `/etc/gitconfig` → `~/.gitconfig` → `.git/config` every
call (there are **534 `ConfigSet::load` call sites** across the CLI; odb.rs already had to
add a `OnceLock` cache for one config key because "calling it once per object lookup
dominated status runtime" — the rest of the codebase never got that treatment). For N
files this is N × (tree-walk + multi-file parse) = super-linear.

## Optimization plan (ranked by impact; all in `grit-lib`)

Each item is verified with Rust tests, targeted integration tests, and **`bench/`** ratios after the change. The fixes are caches/reuse, so the risk is staleness — every cache must invalidate on the relevant mutation (config write, `.gitattributes` change); add or extend library tests when touching attribute/config caching.

### P1 — Cache the gitattributes stack  *(grit-lib/src/attributes.rs)*  — **huge**
`load_gitattributes_for_diff`/`load_gitattributes_for_checkout` rebuild the full stack per
call. The stack (global/info + per-directory `.gitattributes`) is constant for a given
work-tree/tree state within one command. Build it **once** and reuse:
- Expose a reusable `AttributeStack` (or `AttrSource`) handle: `AttributeStack::for_diff(repo)`
  built once, then `stack.attrs_for(path)` per file (cheap lookup, walking only the
  cached per-dir parses).
- Internally memoize per-directory `.gitattributes` parses (the dominant cost in
  `walk_dirs`), keyed by dir + mtime/oid.
- Convert the per-file callers (grep, diff, add, checkout, ls-files, cat-file textconv)
  to build the stack once before the loop.
- **Expected:** `grep` 50 s → < 0.1 s; `add`/`checkout`/`diff --staged` L-scale 6–60× → ~1–2×.

### P2 — Cache the config cascade  *(grit-lib/src/config.rs)*  — **large**
Add a process-lifetime cache to `ConfigSet::load(git_dir, include_system)` keyed by the
resolved file set, invalidated by **mtime** (a `git config` write bumps the local file's
mtime, so correctness is preserved). The 2nd…Nth load becomes a few `stat`s + a clone
instead of a full parse. This transparently fixes all 534 call sites — including the
per-file ones in grep and the per-iteration ones in log/merge/rebase — without touching
each command. (Generalizes the existing `odb` `core_multi_pack_index_cache` pattern.)
- **Expected:** removes the per-file/per-commit config-parse term everywhere; biggest wins
  stack with P1 on grep/add/checkout and shave log/merge/rebase.

### P3 — Index parser allocations  *(grit-lib/src/index.rs)*  — **moderate**
`ls-files` (pure load-index-and-print) is **8.8× at L**, so the parser itself is heavy.
`Index::parse` does `prev_path = entry.path.clone()` for **every** entry (only needed for
v4 path-compression), plus per-entry `String`/`Vec` allocations in `parse_entry`. Make the
prev-path tracking v4-only, parse paths into the entry without the extra clone, and
`with_capacity` the path buffers. Targets the index-load term shared by
status/add/diff/ls-files/write-tree.
- **Expected:** ls-files L 8.8× → ~2×; ~10–20% off status/add/diff index-load time.

### P4 — Prune unchanged subtrees in tree-vs-tree diff  *(grit-lib/src/diff.rs)*  — **moderate**
`log --stat`/`log -p` are 13–16× at scale. `flatten_tree` reads **every** tree object
recursively even when a commit changed a handful of files; C git compares two trees and
**skips subtrees whose OIDs are equal**. Add OID-equality subtree pruning to the
tree-vs-tree path (`diff_trees`/the log/diff-tree callers). (The index-vs-tree path
genuinely needs the full tree since the index is flat, so leave `diff_index_to_tree` —
P1/P3 cover its cost.)
- **Expected:** log --stat/-p over deep history L/H 13–16× → low single digits.

### P5 — Index write & worktree update  *(grit-lib/src/index.rs, checkout/worktree)*  — **targeted**
`add` is **61×** and `checkout` **17.5×** at L beyond what P1/P2 explain. Audit the write
path for O(n²): re-sorting/re-scanning the whole index per added path, or re-serializing
on each entry. Ensure a single sort + single serialize, and a single stat pass.
- **Expected:** add/checkout L → low single digits.

### Transitive: combo commands
`stash`/`merge`/`rebase`/`reset`/`cherry-pick` (44–1030× at L) are CLI orchestration over
checkout + diff + index; they inherit the per-file attribute/config/index costs and are
fixed transitively by P1–P5. No combo-specific work is needed first.

## Sequencing & verification

1. **P1 (attributes cache)** — biggest win, self-contained in `attributes.rs`. Gate: attribute/diff/grep-related Rust tests + `bench/run-everyday.sh`.
2. **P2 (config cache)** — broad win. Gate: config-related Rust tests + full `cargo test -p grit-lib --lib`.
3. **P3 (index parse)** then **P4 (tree-diff pruning)** then **P5 (write path)** — each gated on index/diff/log benchmarks and relevant library tests.

Re-run `bash bench/run-everyday.sh` after each phase; `docs/bench.html` (scale-grouped)
tracks the ratios trending toward parity.

## Clone / commit / push scenarios (`bench/run-ccp.sh`)

Fixed-fixture benchmarks for network-shaped workflows (local clone/push, no external
services). Twelve scenarios over prebuilt repos under `/tmp/grit-bench-ccp`:

| Fixture | Shape |
|---|---|
| **many** | 10k files, 5 commits, packed |
| **many-loose** | 10k files, one commit, loose objects |
| **hist** | 2000 commits on 10 files, packed |
| **large** | three ~40 MB blobs, packed |
| **plain-10k** | 10k files on disk (no repo) for add/commit |

Scenarios: `clone-many`, `clone-many-loose`, `clone-hist`, `clone-large`,
`clone-file-many`, `clone-bare-many`, `commit-touch-many` (`commit -a` on 10k-file
clone), `add-10k`, `commit-10k`, `push-hist`, `push-incr`, `push-large`.

Fixtures are built with **system git**, `core.multiPackIndex=false`, and `gc.auto=0` so
git 2.43 can read packs when used as the reference column (grit may write MIDX v2 that
older git cannot consume) and **many-loose** stays loose (auto-GC would pack 10k
objects during setup).

```bash
bash bench/run-ccp.sh fixtures          # once; scratch under /tmp/grit-bench-ccp
bash bench/run-ccp.sh run                 # hyperfine → bench/results/ccp-<scenario>.json
bash bench/run-ccp.sh run --runs 3        # exactly three timed runs per tool (hyperfine --runs)
BENCH_TOOLS=git,grit-git bash bench/run-ccp.sh run   # omit grit-cli
```

Aggregated baseline metadata: `bench/results/ccp-baseline.json`. Compare **git**,
**grit-git**, and **grit-cli** (modern CLI; no `-q` flags — output redirected during
timing). **grit-cli** skips `clone-bare-many` until bare clone exists; push scenarios
use `origin` as the bare remote because `grit push` has no remote argument.
