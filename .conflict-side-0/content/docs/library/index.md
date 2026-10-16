---
title: Library guide
summary: Use grit-lib from Rust — the same engine that powers the grit CLI.
---

These pages walk through every stable domain in `grit-lib`: repository discovery, typed handles on [`Repository`](rustdoc:grit_lib::repo::Repository), and network or maintenance workflows. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](repository/) | Discover, open, init, typed handles overview |
| [Embedding](embedding/) | Environment, sinks, subprocess injection, concurrency |
| [Config](config/) | [`ConfigHandle`](rustdoc:grit_lib::config::handle::ConfigHandle) get/set |
| [Objects](objects/) | [`Objects`](rustdoc:grit_lib::objects::handle::Objects) read/write |
| [Refs](refs/) | [`References`](rustdoc:grit_lib::refs::handle::References), reflog, backends |
| [Index](staging/) | [`IndexHandle`](rustdoc:grit_lib::index::handle::IndexHandle), stage, write tree |
| [Worktree](worktree/) | Linked worktrees, ignore, attributes |
| [Diff](diff/) | [`Diff`](rustdoc:grit_lib::diff::handle::Diff) tree/index/worktree |
| [Revwalk](revwalk/) | [`RevWalk`](rustdoc:grit_lib::revwalk::handle::RevWalk), merge base |
| [Merge](merge/) | Three-way tree merge, porcelain merge helpers |
| [Network](network/) | [`RepositoryRemotes`](rustdoc:grit_lib::repo_handles::RepositoryRemotes) |
| [Stash](stash/) | [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) |
| [Notes](notes/) | [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes) |
| [Bundle](bundle/) | [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) |
| [Maintenance](maintenance/) | Fsck object rules, hooks |
| [API layout](api-layout/) | Flat → canonical module map |
| [API map](api-map/) | Generated module and type index |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
