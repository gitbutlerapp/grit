# grit-lib

Core library for [Grit](https://github.com/gitbutlerapp/grit), a from-scratch
reimplementation of Git in Rust.

`grit-lib` provides the engine for opening repositories, reading and writing
objects and refs, diffing, merging, fetching, pushing, and the other workflows
the `grit` CLI exposes. It is designed to be **linkable**: embed it in tools,
agents, or services without shelling out to the `git` binary.

Crate-level documentation (architecture, features, and a compiling example) lives
in the [rustdoc](https://docs.rs/grit-lib) for `grit_lib`.

## Design philosophy

- **Correctness first.** Behavior is validated with Rust tests cross-checked
  against the system `git` binary (on-disk formats and wire protocols).
- **Library-first.** Typed errors, no printing from the library, no global
  state, and explicit inputs for time and environment where it matters.
- **Performance.** Hot paths are benchmarked against stock `git`; see the
  project benchmarks docs on [grit-scm.com](https://grit-scm.com/docs/benchmarks).
- **Granular errors.** Public APIs return `thiserror` enums so callers can match
  on failure modes.

## Dependencies

Direct dependencies are declared in [`Cargo.toml`](Cargo.toml) (workspace-pinned
where noted). Highlights:

| Area | Crates |
| ---- | ------ |
| Hashing | `sha1`, `sha2`, `crc32fast` |
| Compression | `flate2` |
| Diff / merge | `similar`, `merge3`, `imara-diff` |
| Text / paths | `regex`, `hex`, `unicode-width`, `icu_normalizer`, `shell-words` |
| Time / TZ | `time`, `tzdb`, `tz` |
| I/O helpers | `tempfile`, `memmap2`, `filetime` |
| Errors | `thiserror` |
| Optional HTTP (`http-ureq` feature) | `ureq`, `base64` |
| Unix-only | `nix`, `rustix` |
| Windows-only | `windows-sys` |

The workspace default denies `unsafe`, but this crate sets `unsafe_code = allow`
in [`Cargo.toml`](Cargo.toml) because several areas use small, reviewed blocks:
Git-compatible date/time (`git_date`, including `libc` FFI), memory-mapped pack
files (`pack_map`), the parallel worker pool, validated UTF-8 object paths
(`objects`), terminal and color detection, index extensions, and (on Windows)
Credential Manager access in `credentials`.

## Quick start

Add the dependency:

```toml
[dependencies]
grit-lib = "0.1"
```

For smart-HTTP remotes:

```toml
grit-lib = { version = "0.1", features = ["http-ureq"] }
```

### Open a repository and read HEAD

```rust
use grit_lib::repo::{init_repository, Repository};
use grit_lib::state::resolve_head;
use grit_lib::RefStorageFormat;

let dir = tempfile::tempdir()?;
let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)?;
let head = resolve_head(&repo.git_dir)?;
let _again = Repository::open(&repo.git_dir, repo.work_tree.as_deref())?;
# Ok::<(), grit_lib::error::Error>(())
```

### Walk the commit graph

```rust
use grit_lib::rev_list::{rev_list, RevListOptions, OutputMode};

let mut opts = RevListOptions::default();
opts.output = OutputMode::OidOnly;
let result = rev_list(&repo.odb, &repo.git_dir, &include, &exclude, &opts)?;
for oid in &result.oids {
    println!("{}", oid.to_hex());
}
```

### Diff two trees

```rust
use grit_lib::diff::{diff_trees, DiffEntry};

let entries: Vec<DiffEntry> = diff_trees(&repo.odb, Some(old_tree), Some(new_tree))?;
for e in &entries {
    println!("{} {}", e.status, e.new_path.as_deref().or(e.old_path.as_deref()).unwrap());
}
```

## Module map

Prefer the [rustdoc](https://docs.rs/grit-lib) domain views (`object_store`,
`references`, `worktree_index`, `revision`, `diffing`, `merging`, `configuration`)
for navigation. Commonly used flat modules include `repo`, `odb`, `objects`, `pack`,
`index`, `refs`, `remote`, `fetch`, `push`, `diff`, `rev_parse`, `rev_list`,
`config`, `bundle`, and `error`.

## License

MIT
