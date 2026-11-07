---
title: Bundle
summary: Offline pack bundles — create, verify, list heads, unbundle, and fetch.
---

Git bundle files combine a header and thin pack for offline transfer. The [`bundle`](rustdoc:grit_lib::bundle) domain implements read/write/verify compatible with system `git bundle`. [`Repository::bundles`](rustdoc:grit_lib::repo::Repository::bundles) returns [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) scoped to one open repository. Every operation has one method; long-running ones take a trailing `&mut dyn ProgressSink` and `&CancelToken` (pass `NullProgress` and `CancelToken::never()` when you need neither).

| Method | Role |
| --- | --- |
| `write` | Write a bundle file from `BundleSpec` tips (atomic replace via `*.bundle.part`); returns the ref count |
| `write_from_revs` | Same, from `bundle create`-style revision arguments (`BundleCreateOptions`: tips, `A..B`, `--all`) |
| `write_to` | Stream a bundle (header, then pack) into any `Write` |
| `plan_create` / `plan_create_from_revs` | Dry run: the refs, prerequisites and object count a write would produce |
| `verify` | Check prerequisites and connectivity (`BundleVerifyReport`) |
| `list_heads` | Parse ref tips from the header |
| `plan_unbundle` | Dry run of `unbundle`: verification and the ref updates it implies |
| `unbundle` | Ingest the pack without updating refs |
| `fetch` | `git fetch` from a bundle file: verify, ingest, and update refs per `FetchOptions` |

Failed or cancelled `write` calls leave an existing destination file unchanged. Grit writes to a sibling `path.bundle.part` and renames onto `path` only after the pack stream succeeds. Missing objects surface as [`BundleError::ObjectNotFound`](rustdoc:grit_lib::bundle::BundleError::ObjectNotFound), not a generic I/O string.

With a files-backed repository `unbundle` installs the pack under `objects/pack/`. With a custom primary object store it writes every object of the pack into that store instead (see [Object stores](object-stores.md#receiving-packs-into-a-custom-store)).

[`write_to`](rustdoc:grit_lib::repo_handles::RepositoryBundles::write_to) streams the pack into the `Write` you pass (wrap files in a `BufWriter`), so the whole pack is never held in memory. When the repository has local packs, deltas and whole objects already stored there are copied verbatim instead of being recomputed, so bundling a packed repository costs about as much as copying its pack.

Cloning from a bundle path goes through the normal clone API; network transport is covered on the [Network](../network/) page.

## Example

<!-- include: grit-examples/src/bin/guide_bundle.rs -->

```bash
cargo run --bin guide_bundle . /tmp/demo.bundle
git bundle verify /tmp/demo.bundle
```

The test runs `git bundle verify` on the file grit wrote.
