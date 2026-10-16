# Library guide

> Use grit-lib from Rust — the same engine that powers the grit CLI.

These pages walk through opening a repository, the object database, refs, the index, diffs, history walks, and network transport using `grit-lib`. Each guide includes a compiled example from `grit-examples` that tests against the system `git` binary.

| Page | Topic |
| --- | --- |
| [Repository](https://grit-scm.com/docs/library/repository/index.md) | Discover, open, config, errors |
| [Embedding](https://grit-scm.com/docs/library/embedding/index.md) | Environment, sinks, subprocess injection, concurrency |
| [Objects](https://grit-scm.com/docs/library/objects/index.md) | Odb read/write, object kinds |
| [Refs](https://grit-scm.com/docs/library/refs/index.md) | Resolve, list, update refs and reflog |
| [Index](https://grit-scm.com/docs/library/staging/index.md) | Read index, stage paths, write trees |
| [Diff](https://grit-scm.com/docs/library/diff/index.md) | Tree, index, and blob diffs |
| [Revwalk](https://grit-scm.com/docs/library/revwalk/index.md) | Rev-parse, rev-list, ranges, merge base |
| [Network](https://grit-scm.com/docs/library/network/index.md) | ls-remote, fetch, push, credentials |
| [API map](https://grit-scm.com/docs/library/api-map/index.md) | Generated module and type index with docs.rs links |

For exhaustive API detail see [grit-lib on docs.rs](https://docs.rs/grit-lib).
