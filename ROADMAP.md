# Grit roadmap

> The living version of this plan is maintained on the Grit Factory dashboard (https://maint.grit-scm.com/roadmap); this file is a snapshot. Items are worked one at a time, in order.

> **Note:** The project is now `grit-lib` and `grit-cli` only. The Git-compatible `grit-git` CLI, the ported upstream shell harness, the vendored Git source tree and the `bench/` scripts (which drove `grit-git`) have been removed. Validation is Rust tests cross-checked against the system `git` binary, plus benchmarks of `grit-lib` and `grit`.

## 1. CI gate for every integration
*Workstream: Foundations*

**Goal.** Integration pushes straight to `main`, and today there is no test CI at all (only `release.yml`). Add a fast, reliable gate.

**Scope.**
- A GitHub Actions workflow on push/PR running: `cargo fmt --check`, `cargo clippy --workspace -D warnings`, `cargo test -p grit-lib -p grit-cli`, and a curated workspace integration subset (odb, refs, index, diff, revs, transport modules).
- Use cargo caching (e.g. Swatinem/rust-cache). Remove the `.cargo/config.toml` `jobs = 2` cap in CI only.
- Delete the stray committed `.PLAN.md.swp`.

**Acceptance.**
- The workflow is green on main and takes under 15 minutes.
- The CI test subset is documented in TESTING.md.
- A broken test demonstrably fails the workflow.

## 2. Benchmark suite and baseline vs Git
*Workstream: Performance*

**Goal.** Make speed measurable everywhere before optimizing.

**Scope.**
- Criterion micro-benchmarks in grit-lib for core operations: SHA-1, inflate/deflate, loose and packed object reads, pack index lookup, delta apply, index read/write, revwalk, rev-parse, tree diff, blob diff, config load, ignore/attribute matching.
- A real-world scenario suite built on `grit` and `grit-lib` (the old `bench/` scripts drove `grit-git` and were removed; `grit-bench` in `grit-utils` is a starting point). It compares grit to `git` with hyperfine on:
  - git.git and a synthetic large repo (≥100k files, deep history, many refs, big packs)
  - clone/fetch over file:// and a local smart-HTTP server
  - status, add, commit, log, diff, blame, rev-list --count, cat-file --batch
- Results go out as machine-readable JSON with the machine description. A committed baseline table lists the grit/git ratio per scenario.

**Acceptance.**
- One command runs each suite.
- Baseline results are committed.
- Ratios are reproducible within ±10% across two runs.

## 3. Documentation site foundation
*Workstream: Docs*

**Goal.** Build a real docs site that every later change keeps up to date.

**Scope.**
- Move user and API docs to a generator, mdBook or equivalent, keeping the existing landing page and blog.
- Sections:
  - Getting started
  - The `grit` CLI (one page per command, with human/`--json`/`--markdown` examples)
  - Library guide (Repository, objects, refs, index, diff, revwalk, network) linking to rustdoc
  - Benchmarks, generated from the suite's JSON, comparing grit to Git per operation
- Build in CI. Fail CI on broken links and on rustdoc warnings for public items.

**Acceptance.**
- The site builds in CI.
- The benchmarks page renders the baseline from the previous item.
- AGENTS.md says how to add docs for a change.

## 4. Fix super-linear hot paths
*Workstream: Performance*

**Status (2026-10-08, acceptance run step 386).** Config/attribute/ignore-once work and pick/merge/stash-apply perf stacks are on the mesh; committed **after** baselines (`grit-utils/baselines/hot-paths-after.json`, `suite-after-LH.json`, `commit-after-LH.json`) and the docs benchmarks page reflect L/H grit-bench + Criterion `hot_paths`. **L-tier gaps vs the 2× bar:** `switch`, `switch-wide`, `merge`, `pick-series`, suite `add`/`status`, and (see suite JSON) `commit` if above 2×. **`pick` at L** is ~1.14× (within 2×). **H/L scaling:** after-run medians still exceed the 1.25× bar for `switch`, `switch-wide`, `pick`, `merge`, and `pick-series` (e.g. switch-wide H/L ≈1.30×, pick-series H/L ≈1.58×). **`grep`, `rebase`, `reset`, and `stash push`** still have no `grit` CLI engine — covered by deferral or proxies (pick-series, Criterion `apply_stash`) as documented on the benchmarks page. **Follow-up perf work** from the acceptance profile is listed under **Follow-up tracking** on `content/docs/benchmarks.md` (no GitHub issues were opened from this factory run — `file_bug_report` is dogfooding-only).

**Goal.** Remove the 100–1000× slowdowns found by the earlier optimization report (grep, stash, merge, rebase, reset, add). Their root cause is in `grit-lib`: reloading `.gitattributes` and the config cascade inside per-file loops.

**Scope.**
- Load attributes, config and ignore once per operation and pass them down.
- Fix every per-path reload found by profiling at the L and H benchmark scales.
- Add regression benchmarks for each fix.

**Acceptance.**
- At L scale, every listed command is within 2× of git; target ≤1.2×.
- The Rust tests show no regressions.
- The benchmark page is updated.

## 5. Prune unused commands from the library — **Done**
*Workstream: Scope*

**Goal.** Stop carrying the email workflow and archive code in grit-lib.

**Scope.**
- Remove from grit-lib: `am.rs`, `mailinfo.rs`, `porcelain/format_patch.rs`, plus archive and other code that served only `grit-git` commands that are out of scope (archive, am, format-patch, mailinfo/mailsplit, imap-send, request-pull, send-email, instaweb, web--browse, daemon, http-backend, scalar, filter-branch, bugreport/diagnose, difftool/mergetool and the svn/p4/cvs bridges).
- Remove library code whose only caller was `grit-git` and that no `grit` command or library user needs.
- Update `docs/v1-scope.md`.

**Acceptance.**
- grit-lib has no email or archive modules.
- The workspace builds and all Rust tests pass.
- Scope docs are updated.

## 6. One hashing abstraction, accelerated SHA-1
*Workstream: Performance*

**Goal.** Make hashing fast and centralized. Plain SHA-1, never sha1dc.

**Scope.**
- Replace the 46 direct `Sha1::new`/`digest` call sites and ad-hoc SHA-256 uses with one `ObjectHasher` / `HashAlgo` API in grit-lib.
- Enable hardware acceleration (SHA-NI on x86_64, the SHA extensions on aarch64) and verify the fastest path is chosen at runtime.
- Hash in parallel where batches allow it (add, index refresh, pack indexing).
- Finish the SHA-256 gaps, e.g. MIDX.

**Acceptance.**
- Hash microbenchmarks match or beat `git hash-object`/OpenSSL throughput.
- No direct SHA-1 crate use remains outside the abstraction.
- SHA-256 tests pass.

## 7. Object database read path
*Workstream: Performance*

**Goal.** Make object reads as fast as Git's.

**Scope.**
- Stop reading whole packs into `Vec` (`read_pack_bytes_cached`). Use memory-mapped or windowed reads; if mmap needs `unsafe`, propose a minimal audited exception to the maintainer first.
- Add a delta-base cache, a faster pack index lookup (fanout plus binary/interpolation search) and MIDX lookups.
- Try zlib-rs or another fast inflate backend.
- Replace the process-global pack/MIDX caches with repository-scoped ones.

**Acceptance.**
- cat-file --batch, rev-list --objects and log -p on git.git and the large repo are within 1.2× of git.
- Peak memory on the large repo is not worse than git by more than 1.5×.

## 8. Library hygiene: no globals, no printing, no exits
*Workstream: Library*

**Status (2026-10-08).** Hygiene ratchet baseline is all-zero with documented `// hygiene:` exemptions (CLI `Environment::capture_process`, `SystemCommandRunner`, immutable lookup tables, pack/MIDX read caches until item 7). `concurrent_repos` covers two parallel scenarios; embedding guide and example document Environment, sinks, and injectable subprocesses. Remaining typed-error migration continues outside this item where APIs still return legacy `Error::Message` shapes.

**Goal.** Make grit-lib linkable: no process-global state, no hidden environment reads, no printing, no subprocesses.

**Scope.**
- A `Repository`/context value owns caches and config, replacing the process-global `OnceLock<Mutex<HashMap>>` caches.
- Read the environment only at the CLI boundary. The 108 `env::var` reads and the `GIT_TEST_*` checks move behind an explicit options struct.
- Remove all 112 `println!`, 87 `eprintln!` and the `process::exit` from grit-lib. Use output sinks and progress traits instead.
- Replace the 221 Git-formatted `fatal:/error:/hint:` strings in grit-lib with typed errors. Render them in grit-cli, in its own words.
- Stop shelling out for `grep`, `iconv`, `kill` and `stty` in the library. `sh` remains only where Git semantics require it (hooks, filters) and goes through an injectable runner.

**Acceptance.**
- grep counts for these patterns in grit-lib are zero, or each remaining one is justified in a comment.
- Two repositories can be used concurrently from one process in a test.
- No regressions in the Rust tests or benchmarks.

## 9. Rust tests: objects, packs, odb
*Workstream: Testing*

**Goal.** Build a safety net before the ODB refactor.

**Scope.**
- Add Rust integration tests for loose objects, packs, idx/rev/midx, deltas, commit-graph, alternates, promisor packs and fsck (drawing scenarios from upstream Git's t1xxx, t5xxx and t6xxx test families where relevant) against the grit-lib API, not the CLI.
- Skip UX and option-compatibility tests.
- Add `cargo llvm-cov` reporting for these modules.

**Acceptance.**
- A mapping table from upstream Git test scenarios to Rust tests lives in TESTING.md (validated by `scripts/tests/test_upstream_mapping.py`).
- Line coverage of odb/pack/midx/commit-graph modules is ≥85% (`grit-lib/coverage-floors.toml` gates the **coverage** CI job).
- Everything runs in CI.

**Status.** Coverage floors at 85% for the core set and each of odb / pack / midx / commit-graph (supporting modules ≥80%); upstream mapping table complete for the object-database scenario set in TESTING.md.

## 10. Pluggable object database backend
*Workstream: Library*

**Goal.** Provide an ODB backend abstraction like the one Git core is introducing.

**Scope.**
- Define `ObjectStore` traits for read, write, exists, info and iteration, with streaming reads for large blobs.
- Implement loose, pack, MIDX and in-memory backends, plus an alternates/composite backend. `Odb` composes them.
- Allow custom backends from outside the crate. Ship an example backend in grit-examples.
- Benchmark: the abstraction must not add measurable overhead.

**Acceptance.**
- All object access goes through the traits.
- An example custom backend passes the object test suite.
- Benchmarks are within noise of the previous item's results.

## 11. Rust tests: refs, reflog, config, ignore, attributes
*Workstream: Testing*

**Goal.** Build a safety net before the ref backend refactor.

**Scope.**
- Add Rust library tests for refs (loose, packed, reftable, symrefs, transactions, locking), reflog, config parsing/writing/includes, ignore rules and attributes.
- Add coverage reporting for these modules.

**Acceptance.**
- The upstream-scenario-to-Rust mapping table is in TESTING.md.
- Coverage of these modules is ≥85%.
- Everything runs in CI.

## 12. Pluggable ref backends
*Workstream: Library*

**Goal.** Put a `RefStore` abstraction behind files (loose + packed) and reftable.

**Scope.**
- Define a trait for reads, iteration with prefix, transactions (prepare/commit/abort with locking) and the reflog.
- Implement the files and reftable backends behind it, removing the 72 ad-hoc `reftable::is_reftable_repo` checks across 20 files.
- Make backend selection come from repository config.

**Acceptance.**
- No `is_reftable_repo` call sites remain outside backend selection.
- Ref tests pass for both backends.
- for-each-ref and update-ref benchmarks within 1.2× of git on 100k refs.

## 13. Network, bundles and packing in the library
*Workstream: Library*

**Goal.** Put all core network and pack functionality in grit-lib, and finish the server side.

**Scope.**
- Implement bundle read, write and verify in grit-lib (the `grit-git` implementation was removed, not moved).
- Bring `transfer::build_pack` up to pack-objects quality (delta reuse, windowing, bitmaps from item 14); it backs both push and the `grit_lib::serve` upload-pack.
- Extend `grit_lib::serve`: shallow/deepen, partial-clone filters, `want-ref`, and running hooks in receive-pack through an injectable runner.
- Make `grit-protocol` call `grit_lib::serve` in-process instead of spawning `grit upload-pack` / `grit receive-pack`.

**Acceptance.**
- Bundles round-trip with `git bundle`.
- grit-protocol never spawns.
- Fetch, push, clone and ls-remote benchmarks are within 1.2× of git.
- The transport test matrices pass.

## 14. Reachability bitmaps
*Workstream: Performance*

**Goal.** Make counting, fetch negotiation, clone serving and `rev-list --count` fast on large repos.

**Scope.**
- Read and write pack and MIDX reachability bitmaps (EWAH), replacing the zero-byte placeholders. Include lookup tables and name-hash caches.
- Use them in rev-list object enumeration and pack generation.

**Acceptance.**
- Bitmaps written by grit are readable by git and vice versa.
- Serving a full clone of git.git through the library is within 1.2× of git.
- `rev-list --count --all` is within 1.2× of git.

## 15. Parallelism
*Workstream: Performance*

**Goal.** Use all cores where Git does, and beyond where it's safe.

**Scope.**
- Parallelize index refresh/status (stat and hash), checkout (write files), pack indexing (`index-pack`), delta search, and diff/blame where independent.
- Make thread counts configurable through options and honor `pack.threads`/`index.threads`.

**Acceptance.**
- Status, checkout, clone and index-pack on the large repo beat git, or are within 1.1×.
- There are no data races, checked with the test suite under `--release` with threads >1.
- Benchmarks are published.

## 16. Rust tests: index, diff, revwalk, revparse, merge
*Workstream: Testing*

**Goal.** Finish Rust coverage for core repository operations.

**Scope.**
- Add Rust library tests for the index (extensions, split index, untracked cache, sparse), diff (tree/blob, rename/copy detection, algorithms), revwalk ordering and limiting, rev-parse syntax, merge (ort-equivalent cases), stash, notes, worktrees and patch-ids.

**Acceptance.**
- The mapping table is in TESTING.md.
- Coverage of these modules is ≥85%.
- Overall grit-lib line coverage is ≥80% and gated in CI so it can't drop.

## 17. Public API design pass
*Workstream: Library*

**Goal.** Make grit-lib a library people choose.

**Scope.**
- A clear `Repository` entry point with domain modules (objects, refs, index, diff, revwalk, config, remote, worktree, stash, notes, bundle).
- Make internal modules private.
- Documentation on every public item, with examples.
- Track the semver surface with `cargo public-api`.
- Fix the stale "Gust library" crate docs.
- Write coverage tests for every public interface.

**Acceptance.**
- `#![deny(missing_docs)]` passes on grit-lib.
- The public API diff is reviewed in CI.
- Every public function has at least one test.
- The docs site library guide is complete.

## 18. grit-cli output contract: --json and --markdown
*Workstream: CLI*

**Goal.** Make every grit-cli command friendly for users, agents and scripts.

**Scope.**
- Add `--markdown` to the existing output model (`output.rs`).
- Document stable JSON schemas per command and version them.
- Add snapshot tests for all three renderings of every command.
- Keep the jq-style `--filter`.

**Acceptance.**
- All 22 commands support `--json` and `--markdown`.
- The schemas are published on the docs site.
- Snapshot tests run in CI.

## 19. grit-cli coverage of core features
*Workstream: CLI*

**Goal.** Expose the library's core capabilities with a modern UX.

**Scope.**
- Add commands, or subcommands of existing ones, for:
  - stash, reflog, worktree, notes, fsck, bundle
  - ls-remote, rev-parse / object inspection (cat/show by spec), revwalk queries
  - gc/maintenance (repack, commit-graph, MIDX, bitmaps)
  - config get/set, ignore check
- Each command ships with `--json`/`--markdown`, docs and tests.

**Acceptance.**
- Every library area in the Direction document has a grit-cli entry point.
- Docs pages exist and benchmarks cover the new commands where relevant.

## 20. Real-world performance campaign
*Workstream: Performance*

**Goal.** Close the remaining gaps on real repositories.

**Scope.**
- Run the scenario suite on large public repos (git.git, linux, chromium-sized synthetic).
- Profile the slowest ratios and fix them one by one with regression benchmarks.
- Publish before/after on the docs benchmarks page.

**Acceptance.**
- Every scenario is within 1.1× of git or faster, or has a documented reason and a follow-up roadmap item.
