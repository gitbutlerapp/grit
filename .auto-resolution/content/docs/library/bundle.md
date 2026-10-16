---
title: Bundle
summary: Offline pack bundles — create, verify, list heads, and unbundle.
---

Git bundle files combine a header and thin pack for offline transfer. The [`bundle`](rustdoc:grit_lib::bundle) domain implements read/write/verify compatible with system `git bundle`. [`Repository::bundles`](rustdoc:grit_lib::repo::Repository) returns [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) scoped to one open repository.

| Method | Role |
| --- | --- |
| [`create`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Write a bundle from [`BundleSpec`](rustdoc:grit_lib::bundle::BundleSpec) tips |
| [`verify`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Check prerequisites and connectivity ([`BundleVerifyReport`](rustdoc:grit_lib::bundle::BundleVerifyReport)) |
| [`list_heads`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Parse ref tips from the header |
| [`unbundle`](rustdoc:grit_lib::repo_handles::RepositoryBundles) | Ingest the pack without updating refs |

For fetch/clone semantics over a bundle path, see [`bundle::bundle_remote`](rustdoc:grit_lib::bundle::bundle_remote). Network transport is covered on the [Network](../network/) page.

## Example

<!-- include: grit-examples/src/bin/guide_bundle.rs -->

```bash
cargo run --bin guide_bundle . /tmp/demo.bundle
git bundle verify /tmp/demo.bundle
```

The test runs `git bundle verify` on the file grit wrote.
