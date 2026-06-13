# Plan: Compiling `grit-lib` and `grit-simple` to WebAssembly

**Scope:** Get `grit-lib` and `grit-simple` to compile (and link) for a
WebAssembly target. **Explicitly out of scope:** `grit-cli` (the `grit` binary),
`grit-http-server`, `grit-protocol`, `grit-utils`, `grit-examples`.

**Status:** Greenfield. There is currently **zero** `cfg(target_arch =
"wasm32")` awareness anywhere in the tree, and `nix`/`libc`/subprocess/socket
usage is woven through the library. This is a substantial, staged effort, not a
one-line `cargo build --target` fix.

---

## 1. Recommendation & target selection

The single most important decision is *which* wasm target we mean, because they
have wildly different cost profiles.

| Capability | `wasm32-wasip1` (WASI preview 1) | `wasm32-unknown-unknown` (browser/JS) |
|---|---|---|
| `std::fs` (read/write/metadata) | ✅ works via WASI | ❌ does not exist — needs a virtual FS |
| `std::env` (vars, `current_dir`) | ✅ (env vars, cwd) | ❌ stubbed/empty |
| `std::time::SystemTime` | ✅ wall clock | ❌ panics/unavailable |
| `std::time::Instant` | ✅ | ⚠️ needs host import |
| `getrandom` (tempfile, etc.) | ✅ native backend | ⚠️ needs `js` feature or custom backend |
| threads | ❌ (no `wasi-threads` in p1 std) | ❌ |
| subprocess (`Command`) | ❌ | ❌ |
| TCP / Unix sockets | ❌ (p1) | ❌ |

**Recommendation: make `wasm32-wasip1` the first milestone, then treat
`wasm32-unknown-unknown` as a second milestone.**

Rationale: on `wasm32-wasip1` the `std` filesystem, environment, and clock APIs
already work, so the *only* things we must remove to compile are the four
genuinely-absent capabilities (subprocess, sockets, threads, FFI to libc time).
That is a tractable feature-gating job. `wasm32-unknown-unknown` additionally
requires abstracting *all of `std::fs` and `std::env`* behind an injected
host interface — a much larger refactor — so it should not block the first
"it compiles" win.

This plan is written so that all the work for milestone 1 is a strict subset of,
and directly reusable by, milestone 2.

---

## 2. Architecture strategy

Three complementary mechanisms, in order of preference:

1. **Cargo feature flags for capability opt-out.** Add features that *remove*
   native-only subsystems from the build. The library already has precedent:
   `http-ureq` is optional and off by default, and the `ureq` client is the only
   `ureq` user (`grit-lib/src/transport/http.rs:50`), cleanly gated. We extend
   this pattern.

2. **`#[cfg]` seams with portable fallbacks.** The codebase already carries
   `#[cfg(unix)]` / `#[cfg(not(unix))]` / `#[cfg(windows)]` seams in ~21 files
   (e.g. `git_date/compat.rs:8-31`, `ident_config.rs:8-49`, the `simple_ipc`
   Unix/stub split at `lib.rs:214-234`). We add a third arm — `#[cfg(target_arch
   = "wasm32")]` — wherever a Unix arm reaches for a syscall.

3. **Trait injection for host-provided I/O.** Networking and credentials are
   *already* abstracted behind traits the embedder implements:
   - `Transport` / `Connection` (`transport.rs:74`, `:113`)
   - `HttpClient` (`transport/http.rs:64`) — `get`/`post` returning `Vec<u8>`
   - `CredentialProvider` (`credentials.rs`)
   On wasm the embedder supplies an `HttpClient` backed by host `fetch()`; the
   built-in `ureq`/SSH/`git://` transports are simply not compiled.

The guiding principle: **the wasm build is the existing library minus the
native I/O subsystems, with behavior exposed through the traits that already
exist.** We are gating code out, not rewriting algorithms. The object store,
pack format, diff/merge, refs, revision walk, and config parsing are all pure
computation and should port unchanged.

### Feature design

Add to `grit-lib/Cargo.toml`:

```toml
[features]
default = ["native"]

# Bundles everything that needs a native OS: subprocess, sockets, libc time,
# nix syscalls, the ureq HTTP client. Off for wasm.
native = ["subprocess", "native-transport", "http-ureq", "libc-time", "nix-syscalls"]

subprocess        = []   # gates Command-spawning subsystems
native-transport  = []   # gates GitDaemonTransport (TCP) + SshTransport
libc-time         = []   # gates the libc/Windows-CRT FFI in git_date
nix-syscalls      = []   # gates direct nix::* calls
# http-ureq already exists
```

wasm builds compile with `--no-default-features` (plus any pure features we
want). Native builds are unchanged because `default = ["native"]` re-enables
everything. **Every existing native consumer keeps working with no flag
changes** — this is the key compatibility constraint.

> Note: `grit-cli` depends on `grit-lib` with default features, so it must keep
> getting the full `native` set. Verify `grit-cli`'s dependency line does not
> set `default-features = false`; it should continue to pull `native`.

---

## 3. Blocker inventory & remediation

Findings from a full sweep of `grit-lib/src`. Each row lists the fix for the
**wasip1 milestone** (compile-out) and the **additional** work for
`unknown-unknown`.

### 3.1 `nix` crate — 6 sites (CRITICAL)

| File:line | Use | Fix |
|---|---|---|
| `unix_process.rs:10` | `signal::kill`, `unistd::Pid` | already `#[cfg(unix)]` module (`lib.rs:250`); ensure not compiled on wasm |
| `simple_ipc.rs:225,232,439` | `pthread_sigmask`, `poll` | `simple_ipc` is already `#[cfg(unix)]` with a non-unix stub (`lib.rs:214-234`); confirm wasm takes the stub arm |
| `ident_config.rs:10,40` | `getuid`, `User::from_uid` | add `#[cfg(target_arch="wasm32")]` arm returning `None`/no passwd lookup, paralleling the existing non-unix arm |
| `untracked_cache.rs:506` | `uname()` | already `#[cfg(unix)]`; add wasm-safe stub (untracked cache disabled on wasm) |
| `repo.rs:1910` | `geteuid()` | wasm arm returns a sentinel (root check is a permissions optimization) |

`nix` becomes an optional dep gated by `nix-syscalls` (and `cfg(unix)`); on wasm
it is not in the dependency graph at all.

### 3.2 `libc` FFI in `git_date` — 3 files, uses `unsafe` (CRITICAL)

- `git_date/compat.rs:9,15,67,86,106` — `extern "C"` `localtime_r`, `mktime`,
  `strftime`, plus the Windows `_localtime64_s` arm.
- `git_date/mod.rs:26` — `extern "C" { fn tzset(); }` + `env::set_var("TZ")`.

The module already has a Unix arm and a Windows arm. Add a **third, pure-Rust
arm** for wasm (and ideally as a `libc-time`-off fallback generally):
- Implement civil-calendar conversion (days→Y/M/D, the `mktime`/`gmtime`
  inverse) in safe Rust. This is a well-known algorithm (Howard Hinnant's
  `days_from_civil` / `civil_from_days`) and is *facts/method*, not protected
  expression, so it is licence-clean under the AGENTS.md rule.
- Timezone: on wasm, drop `tzset`/`TZ`; honor only the explicit numeric offset
  Git already threads through, defaulting to UTC. `localtime` without an OS tz
  database degrades to UTC + explicit offset, which matches Git's behavior when
  `TZ` is unset in a sandbox.

This removes the only `unsafe` FFI in the wasm build, which is desirable given
the workspace `unsafe_code = "forbid"` posture (grit-lib only relaxes it for
this FFI — `grit-lib/Cargo.toml` lint block).

### 3.3 Subprocess spawning — 16+ files (CRITICAL, largest surface)

`std::process::Command` appears in: `transport.rs` (ssh), `signing.rs` (gpg/ssh,
~14 sites), `difftool.rs`, `hooks.rs`, `crlf.rs` (iconv), `userdiff.rs` (grep),
`credentials.rs` (helpers), `interpret_trailers.rs`, `merge_diff.rs`,
`filter_process.rs`, `blame.rs`, `diffstat.rs` (stty), `simple_ipc.rs`,
`submodule_gitdir.rs`, `index.rs`.

Strategy — gate behind the `subprocess` feature, with graceful behavior when off:

- **Pure pass-throughs to external tools** (difftool, mergetool-vimdiff, gpg
  signing/verify, credential *helpers*, filter/clean-smudge processes, hooks,
  iconv in `crlf`, grep in `userdiff`, stty in `diffstat`): wrap each
  `Command`-using function so that with `subprocess` off it returns a typed
  `Error` (e.g. `Error::Unsupported("subprocess")`) or a sane no-op
  (hooks → "no hook run"; stty → fall back to default 80-col width; signing →
  `SigningUnavailable`). Most already return `Result`, so this is adding an
  early `#[cfg(not(feature="subprocess"))]` branch, not deleting call sites.
- **Keep the typed seam.** Per AGENTS.md, the library returns structured
  results; the CLI renders text. So "gpg not available" is a typed error, and
  the host/embedder decides what to do.
- **`signing.rs`** is the biggest single file. Split it so the buffer
  construction / signature-block parsing (pure) stays compiled, and only the
  `Command`-spawning `sign_buffer*` / `verify_*` entry points are gated.
- **`index.rs:2014` / `simple_ipc.rs`** subprocess use is tied to the IPC
  daemon (already Unix-only) and an internal `kill`; both fall out with
  `cfg(unix)` + `subprocess`.

Net: no algorithm is lost; the wasm build simply can't shell out, and says so
through the existing `Result` types.

### 3.4 Transport / sockets — (HIGH)

- `transport.rs:24-26` imports `std::net::{TcpStream, ToSocketAddrs}`;
  `GitDaemonTransport` (`:382`) and `SshTransport` (`:914`) need TCP/subprocess.
  Gate both behind `native-transport`.
- `SmartHttpTransport<C: HttpClient>` (`transport/http.rs:396`) is
  **wasm-compatible as-is** — it only calls `client.get/post`. Keep it.
- `transport/http/ureq_client.rs` stays behind `http-ureq` (already so). Not
  compiled on wasm.
- The fetch/push protocol stacks (`fetch.rs`, `push.rs`, `protocol*.rs`,
  `pkt_line.rs`, `ls_remote.rs`) operate over `Read`/`Write` and the
  `HttpClient` trait — keep compiled. On wasm the embedder provides an
  `HttpClient` over host `fetch()`.
- `simple_ipc.rs` (`std::os::unix::net`) is already Unix-gated → stub on wasm.

### 3.5 Threads — 2 files (MEDIUM)

- `simple_ipc.rs:424,779` — inside the Unix-only IPC server → gone on wasm.
- `index_name_hash_lazy.rs:345` — `thread::spawn` (scoped) to parallelize index
  name-hash computation. Add a `#[cfg(target_arch="wasm32")]` (or
  `cfg(not(feature="threads"))`) arm that computes serially in the current
  thread. The serial path likely already exists as the small-index fallback;
  reuse it.

### 3.6 Time — `SystemTime::now`/`Instant::now`, ~34 files (MEDIUM)

On **wasip1 these work**, so milestone 1 needs no change beyond §3.2's `tzset`
removal. For **unknown-unknown**, route "now" through a single internal
`clock::now()` helper that the embedder can set (default: host import or a
monotonic counter). Audit `reftable.rs:2378`, `simple_ipc.rs` timeouts (Unix,
gone), and `git_date/tm.rs:134` (`SystemTime::now()` for "current time" date
formats) to call the helper.

### 3.7 Filesystem & `filetime` — 56 files (CRITICAL for unknown-unknown only)

- On **wasip1**: `std::fs` works; `filetime::set_file_*` (used in
  `attributes.rs`, `split_index.rs`, `odb.rs`, `config.rs`) — verify `filetime`
  builds for wasip1 (it should via WASI `path_filestat_set_times`); if not, gate
  those mtime writes (they are cache-freshness optimizations, safe to skip).
  Confirmed there is **no `mmap`/`memmap`** usage (plain buffered reads) and
  **no `rayon`** — both good for portability.
- On **unknown-unknown**: there is no filesystem. This requires introducing a
  **`Filesystem` trait** (open/read/write/metadata/readdir/symlink/rename/lock)
  and threading a `&dyn Filesystem` (or a `Vfs` handle on `Repository`) through
  the ~56 call sites. This is the dominant cost of milestone 2 and is
  deliberately deferred. `tempfile` (10+ files) also assumes a real FS and would
  need redirection to the VFS.

### 3.8 Environment & cwd — 42 files (HIGH for unknown-unknown)

- `repo.rs` reads many `GIT_*` vars and, critically, calls
  `env::set_current_dir()` (`repo.rs:2279-2283`). On **wasip1** env + cwd work.
  On **unknown-unknown**, replace process-global cwd with an explicit base path
  carried on `Repository`, and feed env vars from an injected map. Defer to
  milestone 2; flag the `set_current_dir` call as a refactor target since
  process-global cwd is an anti-pattern for embedding anyway.

### 3.9 Transitive dependency notes

- **`getrandom`** (via `tempfile`/`fastrand`): wasip1 has a native backend.
  For unknown-unknown, enable `getrandom`'s `js` feature (or register a custom
  backend) in the wasm build config.
- **`ureq` → `rustls` → `ring`**: `ring` is hard to build for wasm. Irrelevant
  because `http-ureq` is off for wasm. Just confirm it stays out of the graph.
- **`icu_normalizer`** (unicode normalization): pure Rust + baked data tables;
  expected to compile to wasm fine. Verify, since it pulls several `icu_*`
  crates.
- **`nix`, `libc`**: become `cfg(unix)`/feature-gated optionals, absent from the
  wasm graph.

---

## 4. Phased execution plan

### Phase 0 — Tooling & baseline (½ day)
1. `rustup target add wasm32-wasip1 wasm32-unknown-unknown`.
2. Add a `Makefile` target and a `cargo` alias, e.g.
   `make wasm` → `cargo build -p grit-lib --target wasm32-wasip1 --no-default-features`.
3. Add a CI job (GitHub Actions, alongside existing workflows in `.github/`)
   that runs the wasm build so it doesn't regress. Start it allowed-to-fail,
   flip to required once green.
4. Capture the initial error list as the worklist.

### Phase 1 — Feature scaffolding (1 day)
1. Add the `native` / `subprocess` / `native-transport` / `libc-time` /
   `nix-syscalls` features to `grit-lib/Cargo.toml`; make `nix`, `libc`, `ureq`,
   `base64` optional and wire them to features + `cfg(unix)` where appropriate.
2. Make `default = ["native"]`; confirm `cargo build -p grit-cli` and
   `cargo test -p grit-lib` are byte-for-byte unaffected.
3. Confirm `grit-cli` pulls default features (full native set).

### Phase 2 — Compile-out the absent capabilities (the bulk; 3–5 days)
Work module-by-module against the wasip1 error list:
1. Gate `nix` sites (§3.1) with wasm/non-unix arms.
2. Gate `native-transport` (TCP daemon + SSH) and confirm `SmartHttpTransport`
   + protocol stack still compile (§3.4).
3. Gate all `subprocess` sites (§3.3) with typed-error / no-op fallbacks. Tackle
   in dependency order: `hooks`, `signing`, `credentials`, `difftool`/mergetool,
   `filter_process`, `crlf`/`userdiff`/`diffstat`, `merge_diff`, `blame`,
   `submodule_gitdir`.
4. Serial fallback for `index_name_hash_lazy` (§3.5).
5. Pure-Rust `git_date` arm; drop `tzset`/FFI on wasm (§3.2).
6. Verify `filetime` builds for wasip1; gate the mtime writes if not (§3.7).
7. Iterate `cargo build --target wasm32-wasip1 -p grit-lib --no-default-features`
   to zero errors, then zero warnings (the crate denies `unwrap_used`/
   `expect_used`; any new fallback code must comply).

**Exit criterion for milestone 1:** `grit-lib` compiles and links for
`wasm32-wasip1` with `--no-default-features`.

### Phase 3 — `grit-simple` for wasm (1 day)
`grit-simple/src/main.rs` (217 lines) is a `[[bin]]` named `gi` implementing one
command, `shortlog`/`sl`: `Repository::discover`, resolve HEAD, find target
branch, compute commits-ahead, print. Blockers for wasm: `clap`'s
`std::env::args()`, `std::process::exit` (`main.rs:44`), and `println!`/
`eprintln!`.

Refactor (compatible with the existing native binary):
1. Extract the command logic into a library function returning a structured
   value, e.g. `pub fn shortlog(repo: &Repository) -> Result<ShortlogReport>`
   in a new `grit-simple/src/lib.rs`. No printing inside.
2. Keep `main.rs` as the native binary: parse with clap, call `shortlog`, render
   with `println!`, `process::exit`. Gate the whole `[[bin]]`/clap path with
   `#[cfg(not(target_arch = "wasm32"))]` (or build the bin only for native
   targets) so the binary's `env::args`/`exit` never reach wasm.
3. For wasm, build `grit-simple` as a `lib` (add `[lib]`, keep `[[bin]]` for
   native). Expose `shortlog` (and a thin string-rendering helper) as the wasm
   entry point. The actual JS/host binding (wasm-bindgen, or a wasip1 `_start`
   shim) is a thin wrapper added in Phase 5.

**Exit criterion:** `grit-simple` (lib form) compiles for `wasm32-wasip1`.

### Phase 4 — `wasm32-unknown-unknown` (milestone 2; large, 1–2+ weeks)
Only after milestone 1 is green. This is where the heavy refactors land:
1. **`Filesystem`/VFS trait** and thread it through the ~56 `std::fs` sites and
   `tempfile` users (§3.7). Provide an in-memory FS for tests and a JS-backed FS
   for the browser.
2. **Env/cwd injection**: remove `set_current_dir`, carry base path + env map on
   `Repository` (§3.8).
3. **Clock injection** for `SystemTime`/`Instant` (§3.6).
4. **`getrandom` `js` backend** (§3.9).
5. Optional: `wasm-bindgen` bindings + a small JS `HttpClient` over `fetch()`,
   packaged with `wasm-pack`.

### Phase 5 — Packaging & verification (ongoing)
- Smoke test: instantiate the wasm module (wasmtime for wasip1; a Node/browser
  harness for unknown-unknown) and exercise a pure operation (hash-object,
  parse a commit, diff two blobs) end to end.
- Document the embedder contract: required trait impls (`HttpClient`,
  `CredentialProvider`, later `Filesystem`/clock) and which features to disable.

---

## 5. Verification matrix

| Check | Command |
|---|---|
| Native unaffected | `cargo build -p grit-cli && cargo test -p grit-lib --lib` |
| Lib compiles (wasip1) | `cargo build -p grit-lib --target wasm32-wasip1 --no-default-features` |
| Simple compiles (wasip1) | `cargo build -p grit-simple --target wasm32-wasip1 --no-default-features --lib` |
| No new lints | `cargo clippy -p grit-lib --target wasm32-wasip1 --no-default-features` |
| Lib compiles (unknown) | `cargo build -p grit-lib --target wasm32-unknown-unknown --no-default-features` (milestone 2) |
| Runtime smoke | run a pure op under `wasmtime` / Node harness |

Every change in Phases 1–3 must keep the **native** build and `grit-lib` unit
tests green; the wasm features only ever *remove* code from the default build.

---

## 6. Risks & open questions

1. **Target ambiguity.** This plan assumes wasip1-first, browser-second. If the
   real goal is "a JS/browser library now," milestone 2 (VFS/env/clock
   injection) becomes mandatory and the estimate roughly triples. *Confirm the
   intended host.*
2. **Subprocess semantics.** Disabling gpg/hooks/difftool means signed-commit
   creation, hook execution, and external diff/merge tools are unavailable on
   wasm. Confirm that's acceptable (it almost certainly is for a browser/embedded
   use case) vs. needing host-callback shims for any of them.
3. **`filetime` on wasip1** may or may not build cleanly; have the gate ready.
4. **`icu_normalizer` to wasm** — expected fine, but verify early since it's a
   non-trivial dependency tree.
5. **Licensing.** The new pure-Rust `git_date` calendar math and any fallback
   strings must follow the AGENTS.md rule: reimplement method/facts, never copy
   Git's C expression into the MIT-licensed `grit-lib`.

## 7. Effort estimate

| Milestone | Target | Estimate |
|---|---|---|
| 1 | `grit-lib` + `grit-simple` compile for `wasm32-wasip1` | ~1 week |
| 2 | Same for `wasm32-unknown-unknown` (VFS/env/clock injection) | +1–2 weeks |

Milestone 1 is the "it compiles as a wasm build" deliverable. Milestone 2 makes
it useful in a browser without a WASI shim.
