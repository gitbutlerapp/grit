---
title: grit resolve
summary: Resolve revision specs to object ids and types.
group: Plumbing
order: 4
---

## Synopsis

```text
grit resolve [--short] <SPEC>...
```

## Description

Resolves one or more revision specs to an object id, plus the object type and uncompressed size. Type and size follow `git cat-file` semantics, including `refs/replace` when replace refs are enabled (honor `GIT_NO_REPLACE_OBJECTS` to read the stored object instead). Supported forms match the library rev-parse surface: branches and tags, `HEAD~n`, peel suffixes such as `^{tree}`, path suffixes such as `main:path/file`, reflog selectors like `@{1}`, abbreviated object ids, and **change ids** (reverse-hex tokens in `k`–`z`, 4–32 characters — stored header or derived id). See [Change ids — Revisions](../library/change-ids.md#revisions).

Each spec prints on its own line as `OID TYPE` (full hex by default). With `--short`, oids are abbreviated using the same uniqueness rules as elsewhere in grit.

### Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Every spec resolved successfully. |
| `1` | At least one spec failed (unknown revision, ambiguous short object id, or ambiguous change id with candidate rows on stderr). |
| `2` | Usage error (unknown flag or command). |

## Options

| Option | Description |
| --- | --- |
| `<SPEC>…` | One or more revision specs to resolve (required). |
| `--short` | Print abbreviated object ids instead of full hex. |

## Examples

Resolve `HEAD` and its tree:

```console
$ grit resolve HEAD HEAD^{tree}
92501f188ae0815af09a0cef6e121eddda4113cf commit
a1b2c3d4e5f6789012345678901234567890abcd tree
```

Short ids:

```console
$ grit resolve --short main
92501f1 commit
```

## JSON output

Schema: [v1](/schemas/resolve/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `warnings` | array | Non-fatal warnings collected while resolving. |
| `results` | array | One entry per input spec. |
| `results[].spec` | string | Input spec. |
| `results[].oid` | string | Resolved object id (abbreviated when `--short` is set). |
| `results[].kind` | string | `blob`, `tree`, `commit`, or `tag`. |
| `results[].size` | number | Uncompressed object size in bytes. |
| `results[].ref_name` | string | Present when the spec named a ref directly. |

Example:

```json
{
  "results": [
    {
      "spec": "HEAD",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "kind": "commit",
      "size": 234,
      "ref_name": null
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/resolve/v1.json)

Pass `--markdown` for a table of resolved specs, oids, kinds, and sizes.

## See also

- [`grit cat`](cat.md) — show object contents for a spec
- [`grit show`](show.md) — commit-oriented summary with diffstat
