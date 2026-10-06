# Grit v1 — Scope and Exclusions

**Updated:** 2026-10-06

Grit is a Git engine in idiomatic, library-focused Rust.
`grit-lib` is the product; **`grit`** (`grit-cli`) is the primary CLI; **`grit-git`** is an optional Git-compatible CLI.
This document states what the **v1** library release covers and what it deliberately does **not**.

## In scope for v1

v1 targets **commonly used, non-interactive** local and network Git workflows,
driven through `grit-lib` APIs and validated with **Rust tests** and **`bench/`** comparisons against system `git`.

| Area | Notes |
|------|--------|
| Repository open / discovery, `GIT_DIR`/common-dir/work-tree/config load order | Core `Repository` API |
| Linked **worktrees** (add/list/remove/lock/move/repair/prune) | Library + CLI as implemented |
| **Partial clone / promisor**: filters, lazy fetch, backfill | Transport + ODB |
| **Signing** (GPG + SSH): commit/tag sign + verify where implemented | CLI + library hooks |
| **Hooks** (multihook + porcelain integration) | Injectable runners in library |
| **Sparse checkout** (cone + non-cone) | Index + checkout paths |
| **Core workflows**: checkout/restore/reset, merge, cherry-pick/revert/rerere, status, log | Primary UX in `grit-cli` |
| **Maintenance**: `gc`, `repack`, commit-graph / MIDX where implemented | Benchmarked in `bench/` |
| **Submodules** (non-interactive) | Partial; see gaps in issues |
| Transport: smart-HTTP + SSH fetch/push, credential helpers | `grit-lib` transport |

## Explicitly OUT of scope for v1

These are intentional non-goals. They are not bugs; they will not block the v1 tag.

### Interactive UX
- Interactive patch modes: `add -p`, `checkout -p`, `restore -p`, `reset -p`,
  `commit -p`, `stash -p`, `clean -i`.
- `rebase -i` (interactive todo editor), `am --interactive`.
- Any flow whose contract is "spawn an editor / prompt the user and react".

### Email and publishing workflow
- `am`, `format-patch`, `send-email`, `imap-send`, `request-pull`.

### Archive and legacy bridges
- `archive` (tar/zip export as a command surface).
- Subversion, Perforce, and CVS foreign-VCS bridges.

### Upstream test parity
- Passing Git's full upstream shell test suite is **not** a goal.
- Compatibility is **on-disk formats, wire protocols, and behavior covered by Rust tests**.

## Environment notes

- Benchmarks and some integration tests compare against the **system `git`** binary when both are installed.
- **`grit-git`** may implement additional Git-compatible commands; absence from `grit-cli` does not imply the library lacks plumbing.
