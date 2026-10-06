# Kill-Spawns Plan — replacing subprocess spawns with library calls

Grit's mission is a *library-focused* Rust reimplementation of Git, yet today many
operations spawn a child `grit` process where a function call into the same code
would do. This document inventories why we spawn, what can be eliminated, what
must stay, and the order of attack.

**Status:** Phase 0 is done — production code no longer invokes the system `git`
binary anywhere. All remaining spawns are either `grit` re-invoking itself
(via `grit_exe::grit_executable()`) or genuinely external programs (editors,
hooks, ssh, gpg, …).

## Why kill spawns?

- **Performance.** `fork`/`exec` + full CLI re-init (config parse, repo discovery,
  clap) costs milliseconds per call; commands like `commit -v` spawn 2–3 children,
  submodule recursion spawns one per submodule per level.
- **Correctness.** Child processes communicate through argv + env vars + stdout
  text that we then re-parse (e.g. `commit` parses `diff --name-status` output).
  Each round-trip through text is a fidelity risk and was the cause of the
  flag-ordering bug found while removing the system-git spawn from `commit`.
- **Library mission.** Every spawn marks a place where the functionality is not
  callable as a library API. Killing the spawn forces the API to exist.

## Why we spawn today (root causes)

Every self-spawn exists for one of five reasons. Each has an in-process answer:

| # | Root cause | Example | In-process replacement |
|---|-----------|---------|------------------------|
| 1 | **Convenience** — output of another command is wanted as text | `commit -v` runs `grit diff -p` to build the template | Call the diff machinery directly with an `impl Write` sink |
| 2 | **Different repo context** — operate on another repository (submodules) | `rm` runs `grit status --porcelain` inside the submodule | Open a second `Repository` handle; no global state needed |
| 3 | **Process-global state isolation** — env vars (`GIT_DIR`, `GIT_INDEX_FILE`), cwd | most self-spawns set/strip env on the child | Thread an explicit context struct instead of reading `std::env` |
| 4 | **Protocol sidedness** — `upload-pack`/`receive-pack` on the far end of a pipe | local `fetch` spawns `grit upload-pack` | Run the endpoint as a function over an in-memory duplex stream |
| 5 | **Process lifetime** — daemons, background tasks, crash isolation | `maintenance --detach`, credential-cache daemon, bisect running user scripts | Keep as spawns (legitimately a different lifetime) |

Only (1)–(4) are killable. (5) stays.

## Inventory and phases

### Phase 0 — no system `git` (DONE)

- `commands/rm.rs` `submodule_status_dirty()` — was `git status --porcelain`,
  now spawns `grit_executable()`.
- `commands/commit.rs` verbose-template diffs — was the host `/usr/bin/git diff`
  (via the removed `git_binary_for_status()`), now spawns `grit_executable()`
  with `--color=never` and flags ordered before the revision (grit's trailing-flag
  re-parse in `diff.rs` only knows a subset of options — see Phase 1).
- Remaining external-git references are deliberate: `scalar.rs` (interops with a
  system git by design) and `bin/test_httpd.rs` (test-only `git-http-backend`).

### Phase 1 — same-crate function calls (low effort, high frequency)

These spawn `grit` only to capture stdout text. The callee lives in the *same crate* (`grit-git`), so the fix is to factor the command's core into a
`fn run_x(repo: &Repository, opts: X, out: &mut impl Write) -> Result<...>`
that both the CLI entry point and the internal caller use. No crate surgery.

| Spawn site | Spawns | Replace with |
|---|---|---|
| `commands/commit.rs` (verbose template, ×2) | `grit diff [--cached] -p` | patch formatting in `commands/diff.rs` driven by `grit_lib::diff` |
| `commands/commit.rs` (template status) | `grit diff --cached --name-status` | `grit_lib::diff` tree-vs-index entries — the caller already re-parses into `DiffEntry`-shaped data; skip the text round-trip entirely |
| `commands/rm.rs` `submodule_status_dirty` | `grit status --porcelain` in submodule | open submodule `Repository`, reuse `status.rs` worktree/untracked scan; return a bool, never format |
| `commands/status.rs` | `grit submodule summary` | submodule summary as a function |
| `commands/push.rs` (×3) | `update-index --refresh`, `diff-files`, `diff-index` | index refresh + dirty checks via `grit_lib::index`/`diff` |
| `commands/mv.rs`, `reset.rs`, `read_tree.rs` | `update-index` / `read-tree` / `submodule update` helpers | index ops in-process |
| `commands/checkout.rs`/`restore.rs` | `grit apply [--cached]` | factor `apply`'s engine to take buffers |
| `commands/rev_parse.rs`, `range_diff.rs` | `grit rev-parse` / `grit log` | `grit_lib::rev_parse` / log machinery |

Definition of done per row: the spawn is gone, the upstream test files covering
the command stay at their current pass counts (the harness TOMLs in
`data/tests/` are the baseline).

**Watch out:** some upstream tests *observe* child processes (`GIT_TRACE2`
`child_start` counts, e.g. t2080's checkout--worker accounting). Before
removing a spawn, grep `tests/` for trace expectations on that command.

### Phase 2 — pack machinery as streaming APIs (medium effort)

`pack-objects`, `index-pack`, `unpack-objects` are spawned from `gc`, `repack`,
`fetch`, `receive_ingest`, `multi_pack_index`, `maintenance`, and the upload
path. They are spawned because their interfaces are streams (object list in,
pack bytes out). The library shape:

```rust
grit_lib::pack::write_pack(repo, objects, &mut impl Write, PackOpts) -> Result<PackId>
grit_lib::pack::index_pack(repo, &mut impl Read, IndexPackOpts) -> Result<PackId>
grit_lib::pack::unpack_objects(repo, &mut impl Read, UnpackOpts) -> Result<Stats>
```

Most of this logic already exists behind the CLI commands; the work is
separating arg-parsing/stdout from the engine and exposing progress via a
callback instead of stderr. One real subprocess benefit to preserve or
consciously drop: memory isolation for very large packs (a child's allocator
pages are returned to the OS on exit).

### Phase 3 — in-process protocol endpoints for local transport (medium-high)

For `file://`/local fetch and push, we spawn our own `upload-pack` /
`receive-pack` purely to get a pkt-line conversation over pipes
(`fetch_transport.rs`, `file_upload_pack_v2.rs`, `send_pack.rs`,
`ext_transport.rs` fallback, `commands/http_backend.rs`, `grit-protocol`).

The endpoints should be functions over generic streams:

```rust
grit_protocol::upload_pack::serve(repo, &mut impl Read, &mut impl Write, Opts)
grit_protocol::receive_pack::serve(repo, &mut impl Read, &mut impl Write, Opts)
```

Then local transport connects client and server with an in-memory duplex
(thread + `os_pipe` or a ring buffer), and remote transports (ssh, http) keep
the same functions wired to sockets/child stdio. This also collapses the
nested re-invocation in `receive_pack.rs:981`. SSH and `ext::` keep spawning —
the far side is genuinely another machine/program.

### Phase 4 — submodule recursion in-process (high effort, gated on Phase 5)

`clone`, `fetch`, `push`, `submodule update/status`, `read-tree` spawn one
`grit` per submodule (×depth). In-process needs a `Repository` opened per
submodule and **no reliance on process-global env or cwd** anywhere in the call
path — which is the real blocker, hence Phase 5.

### Phase 5 (cross-cutting prerequisite) — kill process-global state

The reason self-spawning is *easy* today is that a child process gets a fresh
env/cwd for free. To call instead of spawn we need:

1. **Env honored only at the boundary.** `GIT_DIR`, `GIT_INDEX_FILE`,
   `GIT_WORK_TREE`, `GIT_CONFIG_PARAMETERS` read once in `main`, snapshotted
   into `Repository`/context structs. Grep target: `std::env::var` outside
   `main.rs` setup (it is pervasive; burn down incrementally, path-by-path as
   each phase needs it).
2. **No cwd dependence.** All library paths absolute, derived from
   `Repository`. (`Command::current_dir` is the tell.)
3. **Output sinks.** Engines write to `&mut impl Write` + progress callbacks,
   never `println!`/stderr directly.
4. **Errors, not exits.** `Result` with typed errors instead of
   `std::process::exit` inside engines, so an in-process callee can't kill the
   caller.
5. **Trace2 spans instead of child processes.** Replace `child_start` events
   with region/span events where tests permit; where tests count children,
   keep the spawn or fix expectations only via the allowed
   `test_expect_failure` flip rule.

### Never killed (by design)

- **User-configured programs:** editor, pager, difftool/mergetool, browsers,
  askpass/credential helpers, gpg/ssh signing, merge drivers, clean/smudge
  filters, `ext::`/remote helpers.
- **Hooks** (`grit-lib/src/hooks.rs`) and shell aliases (`alias.rs` `!` form),
  rebase `exec`, bisect run scripts — running user code in our address space
  is wrong.
- **System utilities:** `stty`, `iconv`, `crontab`/`systemctl`/`launchctl`,
  `man`, ssh.
- **Daemons / detached lifetimes:** maintenance background runs,
  credential-cache daemon, `simple_ipc`, `git daemon` per-connection children
  (though per-connection could become threads later).
- **`scalar`'s system-git calls** — interop with a real git install is the point.

## Suggested order

1. ~~Phase 0~~ (done in this change)
2. Phase 1 rows, one command at a time — each is a self-contained PR with an
   existing test file as its acceptance gate (`t7507`, `t3600`, `t7508`, …).
3. Phase 5 items 3–4 opportunistically as Phase 1 touches each engine.
4. Phase 2 (pack streams) — unblocks gc/repack/fetch/receive hot paths.
5. Phase 3 (local protocol in-process).
6. Phase 4 (submodules), last, after env/cwd hygiene is real.

## Measuring progress

A simple ratchet: `grep -rn "grit_executable()" grit/src grit-lib/src grit-protocol/src | wc -l`
(currently 70 call sites). CI could assert the count never goes up, and each
phase lowers the floor. A companion metric is wall-clock on spawn-heavy tests
(t7400/t7406 submodule suites, t5601 clone).
