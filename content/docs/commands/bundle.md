---
title: grit bundle
summary: Create, verify, and list git bundle files for offline transfer.
group: Remotes
order: 6
---

## Synopsis

```text
grit bundle create <file> <rev>…
grit bundle verify <file>
grit bundle list <file>
```

## Description

Git bundles pack refs and objects into a single file. Use them to move history without a network remote, or to seed a clone from removable media.

`create` walks the given revision specs (same rules as [`grit log`](../log/)), writes prerequisite commits the recipient must already have, lists tip refs in the header, and appends a pack stream. Bundles are detected by the `# v2 git bundle` / `# v3 git bundle` signature, not by the `.bundle` file extension alone.

`verify` checks the pack checksum and, when run inside a repository, confirms prerequisite commits are present and connected to your history.

`list` prints the refs recorded in the bundle header.

You can also treat a bundle path like a remote: [`grit fetch`](../fetch/) and [`grit clone`](../clone/) accept a path to a bundle file.

## Options

`create`:

| Argument | Description |
| --- | --- |
| `<file>` | Output bundle path. |
| `<rev>…` | One or more positive revision specs (commits, branches, ranges). |

`verify` / `list`:

| Argument | Description |
| --- | --- |
| `<file>` | Existing bundle file. |

## Examples

```console
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

## Markdown output

Pass `--markdown` on any `grit bundle` subcommand for the same JSON document wrapped for agents (see [Global options](../global-options/)).

## See also

[grit fetch](../fetch/), [grit clone](../clone/), [grit remote](../remote/)
