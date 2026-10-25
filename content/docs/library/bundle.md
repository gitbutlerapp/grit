---
title: Bundle
summary: Offline pack bundles — create, verify, list heads, and unbundle.
---

Git bundle files combine a header and thin pack for offline transfer. The [`bundle`](rustdoc:grit_lib::bundle) domain implements read/write/verify compatible with system `git bundle`. `Repository::bundles` returns `RepositoryBundles` scoped to one open repository.

| Method | Role |
| --- | --- |
| `create` | Write a bundle from `BundleSpec` tips |
| `verify` | Check prerequisites and connectivity (`BundleVerifyReport`) |
| `list_heads` | Parse ref tips from the header |
| `unbundle` | Ingest the pack without updating refs |

With a files-backed repository `unbundle` installs the pack under `objects/pack/`. With a custom primary object store it writes every object of the pack into that store instead (see [Object stores](object-stores.md#receiving-packs-into-a-custom-store)).

[`write_bundle`](rustdoc:grit_lib::bundle::write_bundle) streams the pack into the `Write` you pass (wrap files in a `BufWriter`), so the whole pack is never held in memory. When the repository has local packs, deltas and whole objects already stored there are copied verbatim instead of being recomputed, so bundling a packed repository costs about as much as copying its pack.

For fetch/clone semantics over a bundle path, see `bundle::bundle_remote`. Network transport is covered on the [Network](../network/) page.

## Example

<!-- include: grit-examples/src/bin/guide_bundle.rs -->

```bash
cargo run --bin guide_bundle . /tmp/demo.bundle
git bundle verify /tmp/demo.bundle
```

The test runs `git bundle verify` on the file grit wrote.
