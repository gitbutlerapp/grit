---
title: grit patch-id
summary: Stable patch-IDs for commits (cherry-pick equivalence).
group: Plumbing
order: 2
---

## Synopsis

```text
grit patch-id <REV>...
grit patch-id --unstable <REV>...
```

## Description

Prints one line per commit that has a defined patch-ID: `<patch-id> <commit>`, matching `git patch-id --stable` on the commit's tree diff. Merge commits and commits with no file changes are omitted.

Uses [`Repository::patch_id`](../../library/maintenance.md) with stable accumulation (file order independent). Pass `--unstable` for the legacy Git 1.9 algorithm.

## Options

| Option | Description |
| --- | --- |
| `--unstable` | Use unstable patch-ID hashing instead of stable (default). |

## Examples

```console
$ grit patch-id HEAD~2..HEAD
a1b2c3d4e5f6789012345678901234567890abcd 0123456789abcdef0123456789abcdef01234567
fedcba0987654321fedcba0987654321fedcba09 abcdef0123456789abcdef0123456789abcdef01
```

## JSON output

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `entries` | array | Each item has `patch_id` and `commit` (full hex object ids). |

Example:

```json
{
  "entries": [
    {
      "patch_id": "a1b2c3d4e5f6789012345678901234567890abcd",
      "commit": "0123456789abcdef0123456789abcdef01234567"
    }
  ]
}
```

## Markdown output

`--markdown` prints a table of patch IDs and commits.

## See also

- [Library maintenance — patch IDs](../../library/maintenance.md)
- [`grit pick`](pick.md)
