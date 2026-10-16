---
title: grit fsck
summary: Check refs, objects, and pack indexes for problems.
group: Maintenance
order: 4
---

## Synopsis

```text
grit fsck
grit fsck --connectivity-only
grit fsck --dangling
grit fsck --no-dangling
grit fsck --strict
grit verify
```

## Description

Verifies repository integrity without invoking the system `git` binary:

- Full run: loose and packed object validation, connectivity from ref roots, refs, multi-pack-index, and commit-graph checks when present.
- With `--connectivity-only`: walk reachability from refs and validate linked objects only (faster; skips store-wide object scans).
- With `--dangling` (or by default on a full run): list objects present in the object database but not reachable from refs.

Progress during long checks is written to **stderr** in human mode (suppressed for `--json` / `--markdown`).

`grit verify` is an alias for `grit fsck`. Exit status **1** when any **error-severity** issue is reported. Warnings alone exit **0** unless `--strict` promotes ref warnings. Unreachable (**dangling**) objects alone do not fail the command.

## Options

| Option | Description |
| --- | --- |
| `--connectivity-only` | Skip full object-store validation; check connectivity from ref roots only. |
| `--dangling` | With `--connectivity-only`, also compute and list unreachable objects (full fsck always includes dangling). |
| `--no-dangling` | Skip unreachable-object listing (matches Git `--no-dangling`). |
| `--strict` | Treat ref-fsck warnings as errors. |

## Examples

Clean repository:

```console
$ grit fsck
Checked 12 objects.
No integrity problems found.
```

Report problems:

```console
$ grit fsck
Checked 8 objects.

Issues:
  missingObject (1):
    deadbeef…: object deadbeef… referenced by abcd1234…

Integrity check failed.
```

## JSON output

Schema: [v1](/schemas/fsck/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `objects_checked` | integer | Loose and packed objects examined during store validation (0 with `--connectivity-only`). |
| `issues` | array | Each entry has `kind` (camelCase msg-id), optional `oid` and `path`, and `detail`. |
| `dangling` | array | Unreachable objects as `{kind, oid}` (`kind` is `commit`, `tree`, `blob`, or `tag`). |

Example:

```json
{
  "objects_checked": 12,
  "issues": [],
  "dangling": []
}
```

Example with a problem:

```json
{
  "objects_checked": 8,
  "issues": [
    {
      "kind": "missingObject",
      "oid": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
      "detail": "object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef referenced by abcd1234abcd1234abcd1234abcd1234abcd1234"
    }
  ],
  "dangling": []
}
```

## Markdown output

Pass `--markdown` for a summary table of issues and dangling objects.

Schema: [v1](/schemas/fsck/v1.json)

## See also

- [`grit gc`](gc.md)
- [`grit bundle verify`](bundle.md)
