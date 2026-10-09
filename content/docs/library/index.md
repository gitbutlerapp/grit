---
title: Library guide
summary: Use grit-lib from Rust — the same engine that powers the grit CLI.
---

These pages walk through opening a repository, the object database, refs, the index, diffs, history walks, and network transport using `grit-lib`. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](repository/) | Discover, open, config, errors |
| [Embedding](embedding/) | Environment, sinks, subprocess injection, concurrency |
| [Objects](objects/) | Odb read/write, object kinds |
| [Refs](refs/) | Resolve, list, update refs and reflog |
| [Index](staging/) | Read index, stage paths, write trees |
| [Diff](diff/) | Tree, index, and blob diffs |
| [Revwalk](revwalk/) | Rev-parse, rev-list, ranges, merge base |
| [Network](network/) | ls-remote, fetch, push, credentials |
| [API map](api-map/) | Generated module and type index with docs.rs links |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
