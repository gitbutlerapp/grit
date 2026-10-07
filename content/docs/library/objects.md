---
title: Objects
summary: ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.
---

Git stores four object kinds grit-lib exposes as [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Every object is named by an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId) (SHA-1 by default). The [`Odb`](rustdoc:grit_lib::odb::Odb) on [`Repository`](rustdoc:grit_lib::repo::Repository) reads loose objects and packed storage transparently.

## Writing

[`Odb::write`](rustdoc:grit_lib::odb::Odb) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree and commit bodies must already be in Git’s text format; use [`parse_tree`](rustdoc:grit_lib::objects::parse_tree) and [`parse_commit`](rustdoc:grit_lib::objects::parse_commit) when reading them back.

[`Odb::hash_object_data`](rustdoc:grit_lib::odb::Odb) computes an id without writing—useful for dry runs and tests.

## Reading

[`Odb::read`](rustdoc:grit_lib::odb::Odb) returns an [`Object`](rustdoc:grit_lib::objects::Object) with `kind` and uncompressed `data`. Packed and loose objects share the same API.

## Example

This example initializes a repository, writes a blob, tree, and commit, verifies structure in memory, and prints the commit id:

<!-- include: grit-examples/src/bin/guide_objects.rs -->

Objects land on disk in standard loose format. System Git can read them:

```
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).
