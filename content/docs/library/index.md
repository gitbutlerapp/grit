---
title: Library guide
summary: Use grit-lib from Rust — the same engine that powers the grit CLI.
---

These pages walk through every stable domain in `grit-lib`: repository discovery, typed handles on `Repository`, and network or maintenance workflows. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](repository/) | Discover, open, init, typed handles overview |
| [Embedding](embedding/) | Environment, sinks, subprocess injection, concurrency |
| [Config](config/) | `Repository::config()` / `Config` get/set |
| [Objects](objects/) | `Objects` read/write |
| [Refs](refs/) | `References`, reflog, backends |
| [Storage backends](storage-backends.md) | Custom object and ref stores, conformance suites |
| [Index](staging/) | `IndexFile`, stage, write tree |
| [Worktree](worktree/) | Linked worktrees, ignore, attributes |
| [Diff](diff/) | `Diff` tree/index/worktree |
| [Revwalk](revwalk/) | `RevWalk`, merge base |
| [Merge](merge/) | Three-way tree merge, porcelain merge helpers |
| [Network](network/) | `Remotes` |
| [Stash](stash/) | `RepositoryStash` |
| [Notes](notes/) | `RepositoryNotes` |
| [Bundle](bundle/) | `Bundles` |
| [Maintenance](maintenance/) | Fsck object rules, hooks |
| [API layout](api-layout/) | Flat → canonical module map |
| [API map](api-map/) | Generated module and type index |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
