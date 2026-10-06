# Grit v1 — Scope and Exclusions

**Updated:** 2026-06-01

Grit is a Git engine in idiomatic Rust with a linkable **`grit-lib`** and a modern
**`grit-cli`** client. **`grit-git`** plus the shell harness in **`tests/`** remain a
regression gate for Git-compatible behavior. This document states what the **v1**
library release covers and — just as importantly — what it deliberately does **not**.

## In scope for v1

v1 targets the **commonly used, non-interactive** local and network Git workflows,
driven through `grit-lib` APIs and covered by Rust tests plus the ported harness
(`tests/`, tracked in the per-test status TOMLs under `data/tests/`).

| Area | Status |
|------|--------|
| Repository open / discovery, `GIT_DIR`/common-dir/work-tree/config load order | ✅ `t1510` 109/109, `t1517` 191/191 |
| Linked **worktrees** (add/list/remove/lock/move/repair/prune) | ✅ Phase 1 |
| **Partial clone / promisor**: `filter=blob:none\|tree:0\|...`, lazy fetch, backfill | ✅ `t0410` 38/38, `t5620` 10/10, `t4067` 9/9; `t5616` 44/47 |
| **Signing** (GPG + SSH): commit/tag sign + verify, `push --signed` | ✅ `t7510` 28/28, `t7528` 26/29, `t7031` 14/14, `t5534` 13/13 |
| **Hooks** (multihook + porcelain integration) | ✅ `t1800` 44/44, `t7503` 22/22, `t5571` 11/11, `t5401/2/3` green |
| **Sparse checkout** (cone + non-cone, read-tree/add/rm/mv/status) | ✅ `t1091` 76/77, `t3705`/`t3602`/`t1011`/`t1090`/`t6428` green; `t1092` 64/106 |
| **Core workflows**: checkout/restore/reset, merge, cherry-pick/revert/rerere, status v1/v2, log | ✅ `t7201` 46/46, `t7102` 38/38, `t7508` 121/126, cherry-pick family green, `t7600-merge` 81/83 |
| **Maintenance**: `gc`, `repack`, `maintenance run` (+ scheduler backends) | ✅ `t6500` 34/35, `t7700` 39/47, `t7900` 71/72 |
| **Submodules** (init/update/sync/deinit/status/foreach, non-interactive) | 🟡 `t7400` 111/124, `t7403` 18/18, `t7406` 43/70, others partial |
| Transport: smart-HTTP + SSH fetch/push, Basic auth + credential helpers | ✅ `t5551`, `t5541`, `t5563`, `t5813`, `t5545`, `t5547` |

(Counts are a snapshot; see `data/tests/` / the dashboards for current numbers.)

## Explicitly OUT of scope for v1

These are intentional non-goals. They are not bugs; they will not block the v1 tag.

### Interactive UX
- Interactive patch modes: `add -p`, `checkout -p`, `restore -p`, `reset -p`,
  `commit -p`, `stash -p`, `clean -i`.
- `rebase -i` (interactive todo editor), `rebase --exec` todo flows, `am --interactive`.
- Any flow whose contract is "spawn an editor / prompt the user and react".

### Complex HTTP authentication
- Digest, NTLM, Negotiate/SPNEGO, Kerberos.
- OAuth device flows / token refresh inside the credential protocol.
- TLS client certificates; curl-grade multistage auth.
- **In scope:** Basic auth + credential helpers + proactive/empty-auth only.

### fsmonitor
- `core.fsmonitor`, the builtin fsmonitor daemon, and any status acceleration that
  depends on it (including untracked-cache *for fsmonitor paths*).

### Tooling / server parity
- Shell completion, `git send-email`, `git shell`, Scalar.
- Server-side daemons: `git daemon`, `http-backend`, server-side `receive-pack`
  (only **client-side** hook invocation is in scope).
- Reftable-by-default and reftable migration UX (reftable read/write exists, but is
  not the default backend).

## Known partial areas (in scope, not fully green)

These work for common cases but have documented gaps deferred past v1:

- **Submodules** — `t2013-checkout-submodule` (16/74) and the `--recurse-submodules`
  checkout family need a submodule populate-on-checkout model that currently conflicts
  with the `submodule deinit` registration chain; reconciling the two is a post-v1
  refactor. `git rm` of gitlinks and recursive update modes are partial.
- **`t1092-sparse-checkout-compatibility`** (64/106) — remaining failures need
  sparse-**index** lazy expansion (`ensure_full_index` regions); grit currently expands
  the sparse index eagerly on load.
- **`t4202-log`** (90/149) — non-graph script formats covered; some pretty/format
  edge cases remain.
- **`t5616-partial-clone`** (44/47) — `restore --recurse-submodules` and an HTTP v2
  multi-round thin-pack negotiation case remain.

## Environment notes (test harness)

- Some harness subtests are executed by the **host system git** (via
  `tests/lib-httpd.sh`'s `REAL_GIT`), not by grit — e.g. `t5551` SHA-256-over-HTTP
  object-format cases on a host with an older system git. These are environment
  artifacts, not grit defects.
- GPG-backed subtests **skip** where `gpg-agent` cannot start in the sandbox; the SSH
  signing paths run for real and the GPG path is validated manually.
- `data/tests/` counts can lag the current binary — re-run a file to get its
  true count rather than trusting the dashboard snapshot.
