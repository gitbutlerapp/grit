---
title: Library guide
summary: Use grit-lib from Rust — the same engine that powers the grit CLI.
---

These pages walk through every stable domain in `grit-lib`: repository discovery, typed handles on `Repository`, and network or maintenance workflows. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](repository/) | Discover, open, init, typed handles overview |
| [Embedding](embedding/) | Environment, sinks, subprocess injection, concurrency |
| [Config](config/) | `ConfigHandle` get/set |
| [Objects](objects/) | `Objects` read/write |
| [Refs](refs/) | `References`, reflog, backends |
| [Index](staging/) | `IndexHandle`, stage, write tree |
| [Worktree](worktree/) | Linked worktrees, ignore, attributes |
| [Diff](diff/) | `Diff` tree/index/worktree |
| [Revwalk](revwalk/) | `RevWalk`, merge base |
| [Merge](merge/) | Three-way tree merge, porcelain merge helpers |
| [Network](network/) | `RepositoryRemotes` |
| [Stash](stash/) | `RepositoryStash` |
| [Notes](notes/) | `RepositoryNotes` |
| [Bundle](bundle/) | `RepositoryBundles` |
| [Maintenance](maintenance/) | Fsck object rules, hooks |
| [API layout](api-layout/) | Flat → canonical module map |
| [API map](api-map/) | Generated module and type index |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
