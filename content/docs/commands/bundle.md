---
title: grit bundle
summary: Create, verify, list, and unbundle git bundle files for offline transfer.
group: Remotes
order: 6
---

## Synopsis

```text
grit bundle create <file> [--all] [--dry-run] [<rev>…]
grit bundle verify <file>
grit bundle list <file>
grit bundle unbundle <file> [--dry-run]
```

## Description

Git bundles pack refs and objects into a single file. Use them to move history without a network remote, or to seed a clone from removable media.

`create` walks the given revision specs (same rules as [`grit log`](../log/)), writes prerequisite commits the recipient must already have, lists tip refs in the header, and appends a pack stream. Bundles are detected by the `# v2 git bundle` / `# v3 git bundle` signature, not by the `.bundle` file extension alone.

When the repository's objects are already packed, `create` copies the existing deltas and whole objects straight from the packs instead of recomputing them, and streams the pack into the file. A bundle of a packed repository is close to a copy of its pack: it takes seconds and its memory is bounded by the pack index, not by the size of the history. Bundles with prerequisites are thin, so an object can be sent as a delta against a prerequisite's blob. In a repository that has packs, objects that are still loose are written whole; run [`grit repack`](../repack/) first if you want them delta-compressed too. A repository with no packs at all gets a full delta search.

`verify` checks the pack checksum and, when run inside a repository, confirms prerequisite commits are present and connected to your history.

`list` prints the refs recorded in the bundle header.

`unbundle` ingests the embedded pack into the current repository (objects only; refs in the header are reported but not updated on disk). Run inside the destination repository.

You can also treat a bundle path like a remote: [`grit fetch`](../fetch/) and [`grit clone`](../clone/) accept a path to a bundle file.

## Options

`create`:

| Argument / flag | Description |
| --- | --- |
| `<file>` | Output bundle path. |
| `--all` | Include `HEAD`, every branch, and every tag in the bundle header (same idea as `git bundle create --all`). Omit `<rev>…` when using this flag. |
| `<rev>…` | One or more positive revision specs (commits, branches, ranges). Not used together with `--all`. |
| `--dry-run` | List refs, prerequisites, and pack object count without writing `<file>`. |

`verify` / `list` / `unbundle`:

| Argument | Description |
| --- | --- |
| `<file>` | Existing bundle file. |
| `-n`, `--limit <N>` | For `list`: print at most `N` header refs (`truncated` in JSON when more exist). |

`unbundle`:

| Option | Description |
| --- | --- |
| `--dry-run` | Verify the bundle and report refs that differ from local tips; do not ingest objects or write refs (`unbundle` never updates refs). |

## Examples

```console
$ grit bundle create backup.bundle --all
Wrote bundle backup.bundle (5 refs).

$ grit bundle create backup.bundle main
Wrote bundle backup.bundle (1 refs).

$ grit bundle verify backup.bundle
backup.bundle is valid
The bundle uses this hash algorithm: sha1
The bundle contains 1 refs:
f4e623f540cd491f8856e9487a3da5f8242307f0 refs/heads/main

$ grit bundle list backup.bundle
f4e623f540cd491f8856e9487a3da5f8242307f0 refs/heads/main
```

## JSON output

Schema: [v1](/schemas/bundle.create/v1.json) · [bundle.list v1](/schemas/bundle.list/v1.json) · [bundle.verify v1](/schemas/bundle.verify/v1.json) · [bundle.unbundle v1](/schemas/bundle.unbundle/v1.json)

`grit bundle verify --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Bundle file path. |
| `ok` | boolean | Whether verification succeeded. |
| `hash_algorithm` | string | Object hash used by the pack (`sha1` or `sha256`). |
| `references` | array | Each entry has `name` and `oid`. |
| `prerequisites` | array of strings | Required commit ids (hex). |
| `missing_prerequisites` | array of strings | Prerequisite ids absent from the current repo (empty when not in a repo or when `ok` is true). |

```json
{
  "path": "backup.bundle",
  "ok": true,
  "hash_algorithm": "sha1",
  "references": [
    {
      "name": "refs/heads/main",
      "oid": "f4e623f540cd491f8856e9487a3da5f8242307f0"
    }
  ],
  "prerequisites": [],
  "missing_prerequisites": []
}
```

`grit bundle list --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Bundle file path. |
| `references` | array | Refs from the bundle header (`name`, `oid`). |
| `truncated` | boolean | `true` when the header held more refs than returned (`-n` / `--limit`). |

```json
{
  "path": "backup.bundle",
  "references": [
    {
      "name": "refs/heads/main",
      "oid": "f4e623f540cd491f8856e9487a3da5f8242307f0"
    }
  ]
}
```

`grit bundle create --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Output bundle path. |
| `refs` | number | Count of refs written to the header. |
| `dry_run` | boolean | `true` when `--dry-run` was used (omitted when false). |
| `object_count` | number | With `--dry-run`, objects that would be packed. |
| `references` | array | With `--dry-run`, planned header refs (`name`, `oid`). |
| `prerequisites` | array of strings | With `--dry-run`, prerequisite commit ids. |

`grit bundle unbundle --json`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `path` | string | Bundle file path. |
| `references` | array | Ref tips from the bundle header (`name`, `oid`). |
| `objects` | number | Objects installed from the pack (omitted on `--dry-run`). |
| `bytes` | number | Pack size in bytes (omitted on `--dry-run`). |
| `dry_run` | boolean | `true` when `--dry-run` was used. |
| `object_count` | number | With `--dry-run`, objects in the embedded pack. |
| `ref_updates` | array | With `--dry-run`, refs that differ from local tips (`name`, `old_oid`, `new_oid`). |
| `ok` | boolean | With `--dry-run`, whether verification succeeded. |
| `missing_prerequisites` | array of strings | With `--dry-run`, prerequisite ids absent locally. |

```json
{
  "path": "backup.bundle",
  "references": [
    {
      "name": "refs/heads/main",
      "oid": "f4e623f540cd491f8856e9487a3da5f8242307f0"
    }
  ],
  "objects": 3,
  "bytes": 512
}
```

## Markdown output

Schema: [v1](/schemas/bundle.create/v1.json)
Pass `--markdown` on any `grit bundle` subcommand for the same JSON document wrapped for agents (see [Global options](../global-options/)).



```text
## Bundle verify

- **path:** `snap.bundle`
- **ok:** true
- **hash_algorithm:** sha1

| Ref | OID |
| --- | --- |
| `HEAD` | `83352a61f6715ec3f0a297c6d6150725154fb833` |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success. |
| `1` | Verify failure or other error. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | `unbundle` missing prerequisite commits ([`missing_prerequisite`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit fetch](../fetch/), [grit clone](../clone/), [grit remote](../remote/)