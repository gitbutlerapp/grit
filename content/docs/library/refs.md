---
title: Refs
summary: Resolve HEAD, list branches and tags, update refs with reflog, and how grit-lib picks loose, packed, or reftable storage.
---

References name commits (and other objects). grit-lib exposes them through [`refs`](rustdoc:grit_lib::refs) and [`reftable`](rustdoc:grit_lib::reftable), with [`reflog`](rustdoc:grit_lib::reflog) for update history. The [`references`](rustdoc:grit_lib::references) module groups these for navigation in rustdoc.

## Resolving HEAD and symbolic refs

[`resolve_head`](rustdoc:grit_lib::state::resolve_head) reads `HEAD` and returns a [`HeadState`](rustdoc:grit_lib::state::HeadState): on a branch (symbolic ref plus commit, if any), detached at a commit, or invalid. To resolve any ref name to an object id, use [`resolve_ref`](rustdoc:grit_lib::refs::resolve_ref), which follows symbolic refs with cycle detection.

[`read_ref_file`](rustdoc:grit_lib::refs::read_ref_file) returns a [`Ref`](rustdoc:grit_lib::refs::Ref) (`Direct` or `Symbolic`) without resolving the whole chain.

## Listing branches and tags

[`list_refs`](rustdoc:grit_lib::refs::list_refs) takes a prefix such as `refs/heads/` or `refs/tags/` and returns sorted `(name, ObjectId)` pairs. Loose refs under `refs/` override stale lines in `packed-refs`, matching Git. [`list_refs_glob`](rustdoc:grit_lib::refs::list_refs_glob) applies pattern matching when you need DWIM-style filtering.

## Creating and updating refs with reflog

[`write_ref`](rustdoc:grit_lib::refs::write_ref) points a ref at a commit (or other object). [`write_symbolic_ref`](rustdoc:grit_lib::refs::write_symbolic_ref) updates symbolic refs such as `HEAD`.

Record history with [`append_reflog`](rustdoc:grit_lib::refs::append_reflog), then read it back with [`read_reflog`](rustdoc:grit_lib::reflog::read_reflog). Each [`ReflogEntry`](rustdoc:grit_lib::reflog::ReflogEntry) carries old and new ids, identity, and message. Batch updates can use [`update_refs`](rustdoc:grit_lib::gc::update_refs) when you need compare-and-swap semantics across many refs.

## Loose, packed, and reftable backends

By default, grit uses the **files** backend: one file per ref under `refs/`, plus an optional `packed-refs` file. [`list_refs`](rustdoc:grit_lib::refs::list_refs) and [`resolve_ref`](rustdoc:grit_lib::refs::resolve_ref) merge packed and loose sources so callers see a single namespace.

When `extensions.refStorage = reftable` is set in config, the same functions dispatch to the **reftable** backend ([`is_reftable_repo`](rustdoc:grit_lib::reftable::is_reftable_repo)). Reflog appends and ref listing go through reftable files instead of `logs/` and loose ref files. You do not choose the backend per call; discovery is automatic from the repository layout and config.

## Example

The program below resolves `HEAD`, lists branches and tags, updates `refs/heads/library-guide-demo`, and appends a reflog entry:

<!-- include: grit-examples/src/bin/guide_refs.rs -->

Run against any repository with at least one commit:

```bash
cargo run --bin guide_refs /path/to/repo
git -C /path/to/repo rev-parse refs/heads/library-guide-demo
git -C /path/to/repo reflog show refs/heads/library-guide-demo
```
