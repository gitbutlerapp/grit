---
title: Library guide
summary: Use grit-lib from Rust — the same engine that powers the grit CLI.
---

These pages walk through opening a repository, the object database, history walks, and network transport using `grit-lib`. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](repository/) | Discover, open, config, errors |
| [Objects](objects/) | Odb read/write, object kinds |
| [Revwalk](revwalk/) | Rev-parse, rev-list, ranges, merge base |
| [Network](network/) | ls-remote, fetch, push, credentials |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
