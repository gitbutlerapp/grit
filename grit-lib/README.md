# grit-lib

Core library for [Grit](https://github.com/gitbutlerapp/grit), a from-scratch
reimplementation of Git in Rust.

`grit-lib` provides the low-level building blocks for working with Git
repositories: object storage, index manipulation, refs, diffing, merge,
revision walking, configuration, and more. It is the engine behind the `grit`
CLI but is designed to be usable as a standalone library for anyone who wants
to read, write, or inspect Git repositories from Rust without shelling out to
`git`.

## Design philosophy

- **Correctness first.** Behavior is validated with Rust tests cross-checked
  against the system `git` binary (on-disk formats and wire protocols).
  Edge cases (empty trees, encoding quirks, unusual ref names) are handled the
  way Git handles them.
- **No unsafe code.** The workspace forbids `unsafe` — all I/O goes through
  safe Rust and standard-library abstractions.
- **Minimal dependencies.** The library leans on a small set of widely-used
  crates (`sha1`, `flate2`, `hex`, `similar`, `regex`, `time`, `tempfile`,
  `thiserror`) and nothing else.
- **Granular errors.** Every module returns typed errors via `thiserror` so
  callers can match on exactly what went wrong.

## Quick start

Add the dependency:

```toml
[dependencies]
grit-lib = "0.1"
```

### Open a repository and read an object

```rust
use grit_lib::repo::Repository;

let repo = Repository::discover(".")?;
let head = grit_lib::state::resolve_head(&repo.git_dir)?;

if let Some(oid) = head.oid() {
    let obj = repo.odb.read(oid)?;
    println!("{} {} bytes", obj.kind, obj.data.len());
}
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

### Read and write the index

```rust
use grit_lib::index::Index;

let mut index = Index::load(&repo.index_path())?;
index.add_or_replace(entry);
index.sort();
index.write(&repo.index_path())?;
```

## Module overview

| Module | Purpose |
|--------|---------|
| `repo` | Discover and open repositories |
| `objects` | Object model — `ObjectId`, `ObjectKind`, `CommitData`, `TagData`, `TreeEntry` |
| `odb` | Loose-object database: read, write, hash, existence checks |
| `pack` | Pack file and pack index reading, verification, delta application |
| `index` | Index (staging area) parsing, writing, and entry manipulation |
| `refs` | Loose reference reading, writing, deletion, listing |
| `reflog` | Reflog entry parsing, appending, expiry |
| `state` | HEAD resolution, in-progress operation detection |
| `diff` | Tree-to-tree, index-to-tree, and worktree diffing with rename/copy detection |
| `merge_base` | Merge-base computation, ancestry checks |
| `merge_file` | Three-way line-level merge with conflict styles |
| `rev_parse` | Revision string parsing (`HEAD~3`, `v1.0^{commit}`, etc.) |
| `rev_list` | Commit graph traversal with filtering and ordering |
| `name_rev` | Map commit OIDs to human-readable ref-based names |
| `config` | Multi-scope configuration loading and typed value parsing |
| `ignore` | `.gitignore` pattern matching |
| `hooks` | Hook resolution and execution |
| `write_tree` | Build tree objects from index entries |
| `patch_ids` | Patch ID computation for cherry-pick detection |
| `check_ref_format` | Ref name validation per Git rules |
| `stripspace` | Whitespace and comment normalization |
| `fmt_merge_msg` | Merge commit message formatting |
| `remote` | Typed remotes: list refs, fetch, and push over local, git, ssh, and HTTP |
| `unpack_objects` | Unpack a pack stream into loose objects |
| `error` | Typed error enum covering all failure modes |

## License

MIT
