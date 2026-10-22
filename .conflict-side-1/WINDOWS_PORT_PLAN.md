# Windows Compilation Plan — `grit-lib` + `grit-cli`

Goal: get `grit-lib` and `grit-cli` to **compile for Windows**. The other
workspace members (`grit-http-server`, `grit-protocol`, `grit-utils`,
`grit-examples`) are explicitly **out of scope**.

`grit-cli` (the `grit` binary) contains **no platform-specific code of its
own** — it only re-exports `grit-lib` operations through `clap`. So "make
grit-cli build on Windows" reduces entirely to "make `grit-lib` build on
Windows with its default feature set" (`http-ureq` off).

## Current state

The port is already ~95% complete. Most platform-specific call sites are
already paired with `#[cfg(unix)]` / `#[cfg(not(unix))]` branches, and the
infrastructure modules are gated:

- `git_date/compat.rs` — fully cross-platform: `time_t`/`tm` and
  `localtime_r`/`mktime`/`strftime` route to `libc` on Unix and the MSVC CRT
  (`_localtime64_s`, `_mktime64`, `strftime`) on Windows. `gmtime` is pure Rust.
- `simple_ipc` and `unix_process` are `#[cfg(unix)]` modules with
  `#[cfg(not(unix))]` stubs in `lib.rs` (Unix-domain-socket IPC and
  `kill(2)`-based process control are Unix-only).
- Already-ported with both branches: `ident_resolve`, `signing`, `mailmap`,
  `index`, `crlf`, `attributes`, `untracked_cache`, `ident_config`, `repo`,
  `split_index`, `shared_repo`, `porcelain/stash`, `porcelain/status`, `odb`,
  `hooks`.

## Verification method

There is no Windows toolchain in this environment, but `cargo check` does not
link, so the Windows **std** target is enough to surface every type/path error:

```sh
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu -p grit-cli
```

(`grit-cli` pulls in `grit-lib` with default features, so this checks both.)

## Remaining compile errors and their fixes

A clean cross-check surfaced exactly **8 errors across 3 files** — the last
un-gated Unix call sites. Each is fixed with the same `#[cfg]` pattern already
used throughout the crate.

### 1. `grit-lib/src/porcelain/checkout.rs` (5 errors)

`apply_index_file_mode` and `write_to_worktree` used
`std::os::unix::fs::PermissionsExt::set_mode` and
`std::os::unix::fs::symlink` unconditionally.

- **`apply_index_file_mode`**: gate the `PermissionsExt`/`set_mode` body in
  `#[cfg(unix)]`; on Windows it is a no-op (no POSIX mode bits — the executable
  bit is not represented in the filesystem). Mirrors `porcelain/stash.rs`.
- **`write_to_worktree`**: gate the `symlink` call in `#[cfg(unix)]`; on Windows
  write the symlink target as a regular file (unprivileged symlink creation is
  unavailable) so the worktree stays populated. Gate the executable-bit
  `set_mode` block in `#[cfg(unix)]`.

### 2. `grit-lib/src/diff.rs` (2 errors)

`worktree_content_matches_index_oid` used
`std::os::unix::ffi::OsStrExt::as_bytes` to hash a symlink target.

- Extract a `symlink_target_bytes(&Path) -> Vec<u8>` helper: raw `OsStr` bytes
  on Unix, lossy UTF-8 on Windows (WTF-8 `OsStr` has no stable byte view;
  symlink targets in Git trees are UTF-8 in practice). Mirrors the
  `porcelain/stash.rs` `symlink_target_to_bytes` pattern.

### 3. `grit-lib/src/difftool.rs` (1 error)

The symlink-backed difftool checkout optimisation called
`std::os::unix::fs::symlink` unconditionally.

- Gate the whole `if use_symlinks { … symlink … }` fast-path in `#[cfg(unix)]`;
  on Windows it falls through to the existing file-copy path below.

> Status: **all three files have been fixed in this branch** and
> `cargo check --target x86_64-pc-windows-gnu -p grit-cli` now completes with
> **no errors** (warnings only). The host build (`cargo check -p grit-cli`)
> remains clean.

## Behavioural caveats on Windows (intentional, document but don't block)

These keep the crate compiling and functional; exact Git semantics for these
edge cases are a follow-up, not a compile blocker:

- **Symlinks**: `checkout`/`stash` materialise mode-`120000` entries as regular
  files containing the target path rather than real symlinks. `difftool` copies
  instead of symlinking.
- **Executable bit**: not stored by the filesystem, so mode application is a
  no-op. The index already records mode `100644`/`100755` from the tree; only
  worktree application differs.
- **`untracked_cache`**: uses `uname(2)` (`nix`) under `#[cfg(unix)]` for cache
  validity; the `#[cfg(not(unix))]` branch already disables/short-circuits it.
- **Default identity**: `ident_config` resolves the user via `getuid`/passwd on
  Unix; the Windows branch already falls back to env-based resolution.

## Warning clean-up — done

The Windows cross-check originally emitted ~12 warnings in already-ported files
where an import/binding is only used on Unix. All are now resolved by gating the
imports (`#[cfg(unix)] use …`) or, for parameters, `#[cfg_attr(not(unix),
allow(unused_variables))]`:

- `porcelain/stash.rs` — `MODE_EXECUTABLE` import gated; symlink restore now
  writes the target as a regular file on Windows (also uses `target`).
- `attributes.rs`, `ident_resolve.rs`, `shared_repo.rs`, `mailmap.rs` — unused
  imports gated to `#[cfg(unix)]`.
- `hooks.rs` — `traditional_hook_candidate` annotated
  `#[cfg_attr(not(unix), allow(unused_variables))]`.
- `repo.rs` — dropped the `#[cfg(unix)]` on the `ensure_valid_ownership` call so
  the existing `#[cfg(not(unix))]` stub is used; this removes both the unused
  `gitfile` binding **and** the dead-stub warnings at once.
- `split_index.rs` — `calc_shared_perm` gated to `#[cfg(unix)]`.
- `git_date/compat.rs` — `#[allow(non_camel_case_types)]` on the Windows
  `time_t` alias (mirrors the C / `libc` spelling).

After this, both host and Windows checks emit only the **pre-existing**
`commit_graph_file.rs` `base_layers_declared` dead-field warning, which is
present on host independent of the port and is out of scope here.

## Dependency hygiene — done

`nix` (signals, poll, uid, `uname`, Unix sockets) is used exclusively behind
`#[cfg(unix)]`, so it is now declared under a target table in
`grit-lib/Cargo.toml` and is not built on Windows:

```toml
[target.'cfg(unix)'.dependencies]
nix.workspace = true
```

`libc` deliberately stays a general dependency: `refs.rs` compares
`e.raw_os_error()` against `libc::EISDIR`/`ENOTDIR`/`EPERM` **un-gated**, and
those errno constants are defined by `libc` on Windows too.

## MSVC vs GNU note

Validation uses `x86_64-pc-windows-gnu` because `cargo check` needs only the
target std, not a linker. The one place that actually **links** against the CRT
is `git_date/compat.rs` (`_localtime64_s`, `_mktime64`, `strftime`). These exist
in both mingw's `msvcrt` and MSVC's `ucrt`, so a real
`x86_64-pc-windows-msvc` **build** (not just check) should be run on a Windows
runner / via `cargo-xwin` as a final gate before declaring the port done.

## Definition of done

1. `cargo check --target x86_64-pc-windows-gnu -p grit-cli` — no errors. ✅ (done in this branch)
2. `cargo check -p grit-cli` (host) — still clean. ✅
3. (Recommended) `cargo build --target x86_64-pc-windows-msvc -p grit-cli` on
   a Windows runner to validate the `git_date` CRT FFI links.
4. Warning clean-up + `nix` target-gating — done (see sections above).
