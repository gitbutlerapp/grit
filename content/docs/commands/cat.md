---
title: grit cat
summary: Show blob, tree, commit, or tag contents for a revision spec.
group: Plumbing
order: 5
---

## Synopsis

```text
grit cat <SPEC>
```

## Description

Shows the contents of the object named by a revision spec:

- **Blob** — raw bytes written to stdout (no extra framing).
- **Tree** — one line per entry like `git cat-file -p` (six-digit directory modes such as `040000`, then type, oid, tab, path).
- **Commit** or **tag** — the canonical object payload (same bytes as `git cat-file -p`).

Object bytes and metadata honor `refs/replace` the same way as Git (unless `GIT_NO_REPLACE_OBJECTS` is set). Supported specs are the same as [`grit resolve`](resolve.md): peeled tips, `rev:path`, short oids, and reflog selectors.

## Options

| Option | Description |
| --- | --- |
| `<SPEC>` | Object or revision spec to display (required). |

## Examples

Show a tracked file at `HEAD`:

```console
$ grit cat HEAD:README.md
# hello
```

Show a commit payload:

```console
$ grit cat HEAD^{commit}
tree a1b2c3d4e5f6789012345678901234567890abcd
parent 919c45f33de5e5c0bd05f8ffb089f697f4644976
author Ada Lovelace <ada@example.com> 1700000000 +0000
committer Ada Lovelace <ada@example.com> 1700000000 +0000

initial
```

## JSON output

Schema: [v1](/schemas/cat/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `warnings` | array | Non-fatal warnings while resolving or reading. |
| `spec` | string | Input spec. |
| `oid` | string | Resolved object id (full hex). |
| `kind` | string | `blob`, `tree`, `commit`, or `tag`. |
| `size` | number | Uncompressed object size in bytes. |
| `content` | object | Tagged payload; shape depends on `kind`. |
| `content.kind` | string | Discriminator: `blob`, `tree`, `commit`, or `tag`. |
| `content.encoding` | string | For blobs: `utf-8` or `base64`. |
| `content.text` | string | UTF-8 blob or commit/tag body when applicable. |
| `content.data_base64` | string | Blob bytes when not UTF-8 (omitted when the blob is ≥ 1 MiB). |
| `content.omitted_inline` | boolean | When `true`, the blob is too large to embed; human mode streams bytes to stdout. |
| `content.entries` | array | Tree lines with `mode`, `type`, `oid`, and `path`. |

Example (blob):

```json
{
  "spec": "HEAD:README.md",
  "oid": "6ff87c695474aa159952762750a82000",
  "kind": "blob",
  "size": 8,
  "content": {
    "kind": "blob",
    "encoding": "utf-8",
    "text": "# hello\n"
  }
}
```

## Markdown output

Schema: [v1](/schemas/cat/v1.json)

Pass `--markdown` for a short summary and a fenced block (or table for trees) of the object contents. Binary blobs are summarized in Markdown; use `--json` for base64 payload.



```text
## Cat

- **spec:** `HEAD:tracked.txt`
- **oid:** `b641243fee370fbfe093daae08234ecc4b993d52`
- **kind:** blob
- **size:** 11

Blob body follows in a `text` code fence when stdout is a file.
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

- [`grit resolve`](resolve.md) — resolve specs to ids and types without printing contents
- [`grit show`](show.md) — human-oriented commit and tag summaries
