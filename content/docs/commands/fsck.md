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
grit fsck --verbose
grit verify
```

## Description

Verifies repository integrity without invoking the system `git` binary:

- Full run: loose and packed object validation, connectivity from ref roots, refs, multi-pack-index, and commit-graph checks when present.
- With `--connectivity-only`: walk reachability from refs and validate linked objects only (faster; skips store-wide object scans).
- With `--dangling` (or by default on a full run): list objects present in the object database but not reachable from refs.

Ref checks include loose ref files, symrefs and `packed-refs`. A `packed-refs` file whose header claims the `sorted` trait while its records are out of order is an error (`packedRefUnsorted`, at the first out-of-order line). Normal reads trust that trait and binary-search the file, as Git does, so refs on the wrong side of the disorder can be invisible until the file is rewritten (for example by `grit gc`).

Progress during long checks is written to **stderr** in human mode (suppressed for `--json` / `--markdown`).

Packed objects are verified in a single streaming pass over each pack's delta trees, spread over `--threads` workers, so memory stays close to `git fsck` even on large repositories (git/git, about 422,000 objects: roughly 640 MB peak for grit against 510 MB for git, in about half git's time). An object whose delta base is corrupt or missing is reported as `corruptObject` together with every object built on top of it.

`grit verify` is an alias for `grit fsck`. Exit status **1** when any **error-severity** issue is reported. Warnings alone exit **0** unless `--strict` promotes ref warnings. Unreachable (**dangling**) objects alone do not fail the command.

## Options

| Option | Description |
| --- | --- |
| `--connectivity-only` | Skip full object-store validation; check connectivity from ref roots only. |
| `--dangling` | With `--connectivity-only`, also compute and list unreachable objects (full fsck always includes dangling). |
| `--no-dangling` | Skip unreachable-object listing (matches Git `--no-dangling`). |
| `--strict` | Treat ref-fsck warnings as errors. |
| `-v`, `--verbose` | Include info-level object diagnostics in human output (Git hides these by default). |

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
| `ok` | boolean | `true` when no error-severity issues were found (warnings and info alone still yield `true`). |
| `objects_checked` | integer | Loose and packed objects examined during store validation (0 with `--connectivity-only`). |
| `issues` | array | Each entry has `kind` (camelCase msg-id), `severity` (`error`, `warning`, or `info`), optional `oid` and `path`, and `detail`. Known kinds include `missingObject`, `hasSha1`, and `sha1Collision` (optional sha1dc signal; default severity is **warning**, matching Git’s tolerance for SHAttered objects). |
| `dangling` | array | Unreachable objects as `{kind, oid}` (`kind` is `commit`, `tree`, `blob`, or `tag`). |

Example:

```json
{
  "ok": true,
  "objects_checked": 12,
  "issues": [],
  "dangling": []
}
```

Example with a problem:

```json
{
  "ok": false,
  "objects_checked": 8,
  "issues": [
    {
      "kind": "missingObject",
      "severity": "error",
      "oid": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
      "detail": "object deadbeefdeadbeefdeadbeefdeadbeefdeadbeef referenced by abcd1234abcd1234abcd1234abcd1234abcd1234"
    }
  ],
  "dangling": []
}
```

When configured to surface sha1dc diagnostics, `sha1Collision` is a **warning**
by default (strict mode promotes it to an error). Detection covers the object
header and payload together, as in Git, so a blob that merely contains colliding
bytes, such as the SHAttered PDFs in Git's own test suite, is an ordinary object:

```json
{
  "ok": true,
  "objects_checked": 1,
  "issues": [
    {
      "kind": "sha1Collision",
      "severity": "warning",
      "oid": "ba9aaa145ccd24ef760cf31c74d8f7ca1a2e47b0",
      "detail": "SHA-1 chosen-prefix collision: …"
    }
  ],
  "dangling": []
}
```

## Markdown output

Pass `--markdown` for a summary with `ok`, a table of issues (including severity), and dangling objects. Info-level issues appear in `--json` and `--markdown` but are omitted from default human output unless `--verbose` is set.

Schema: [v1](/schemas/fsck/v1.json)




```text
## Fsck

- **ok:** true
- **objects_checked:** 9

### Dangling

- `blob` `cc7c1250b368e807ba98143f3b0b47c3cd8b74fe`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | No error-severity issues (warnings alone are OK unless `--strict`). |
| `1` | At least one error-severity issue ([`failure`](../exit-codes/)). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

Unreachable objects alone do not fail the command. See [Exit codes](../exit-codes/).


## See also

- [`grit gc`](gc.md)
- [`grit bundle verify`](bundle.md)
