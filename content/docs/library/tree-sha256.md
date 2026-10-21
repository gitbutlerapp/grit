---
title: Tree SHA-256
summary: Content digest of a revision's tree for signed tags and verification.
---

The [tree-sha256](https://github.com/schacon/tree-sha256) digest hashes file and submodule **contents** with SHA-256 (not Git's SHA-1 object names). Grit exposes it on [`Repository::tree_sha256`](rustdoc:grit_lib::repo::Repository::tree_sha256).

## Compute a digest

```rust
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::tree_sha256::TreeSha256Options;

# fn main() -> grit_lib::error::Result<()> {
let dir = tempfile::tempdir()?;
let repo = Repository::init(dir.path(), &InitOptions::default())?;
# let head = repo.odb().write_loose_materialize(
#     grit_lib::objects::ObjectKind::Commit,
#     b"tree 0000000000000000000000000000000000000000\n\n",
# )?;
let report = repo.tree_sha256(head, &TreeSha256Options::default())?;
println!("{}", report.digest);
# Ok(())
# }
```

[`TreeSha256Report`](rustdoc:grit_lib::tree_sha256::TreeSha256Report) includes counts and any submodule paths recorded as `cycle:` or `excluded:` (see the upstream spec).

## Example program

<!-- include: grit-examples/src/bin/guide_tree_sha256.rs -->

## Missing submodules

When a gitlink submodule is not checked out or its commit is absent, hashing returns [`Error::TreeSha256MissingSubmodules`](rustdoc:grit_lib::error::Error::TreeSha256MissingSubmodules) listing every path.
