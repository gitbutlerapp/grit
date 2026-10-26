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

Prints one line per commit that has a defined patch-ID: `<patch-id> <commit>`, matching `git show --format= --no-ext-diff -U3 <REV> | git patch-id --stable` on the commit's tree diff. Text hunks include three unchanged context lines on each side; nearby context contributes to the ID, while unchanged lines outside the hunks do not. Merge commits and commits with no file changes are omitted.

Uses [`Repository::patch_id`](../../library/maintenance.md) with stable accumulation (file order independent). Pass `--unstable` for the legacy Git 1.9 algorithm.

## Options

| Option | Description |
| --- | --- |
| `--unstable` | Use unstable patch-ID hashing instead of stable (default). |

## Examples

```console
$ grit patch-id HEAD~2 HEAD~1 HEAD
a1b2c3d4e5f6789012345678901234567890abcd 0123456789abcdef0123456789abcdef01234567
fedcba0987654321fedcba0987654321fedcba09 abcdef0123456789abcdef0123456789abcdef01
```

Each argument is one commit (or other single object); revision ranges such as `HEAD~2..HEAD` are not expanded.

## JSON output

Schema: [v1](/schemas/patch-id/v1.json)

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

Schema: [v1](/schemas/patch-id/v1.json)

`--markdown` prints a table of patch IDs and commits.



```text
## Patch IDs

| Patch ID | Commit |
| --- | --- |
| `db20cbf19404d5a762136b964e1df57a06af6104` | `83352a61f6715ec3f0a297c6d6150725154fb833` |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## Benchmarking

Compare the same commit with the installed system Git:

```bash
/usr/bin/time -v grit patch-id HEAD --json
/usr/bin/time -v sh -c 'git show --format= --no-ext-diff -U3 HEAD | git patch-id --stable'
```

Use a large text file with sparse edits to measure hunk traversal rather than only process startup. Record wall time and peak resident memory with warm object caches; use the `dist` profile when building grit locally.

Measured on 2026-10-26 on Linux x86_64 (4 vCPUs, AMD EPYC 9V74), using a 14 MB file with 200,000 unique lines and ten replacements spaced 20,000 lines apart. Both grit binaries use the `dist` profile. Results are medians of 21 interleaved warm-cache runs per command after one warm-up; peak RSS comes from `/usr/bin/time -v` for each invocation.

| Implementation | Wall time | Median peak RSS | Peak RSS range |
| --- | ---: | ---: | ---: |
| System Git 2.55.0 | 62.91 ms | 62,604 KiB | 61,696–63,240 KiB |
| Grit before fix (`fa6a9fd55`) | 175.43 ms | 100,732 KiB | 100,008–101,892 KiB |
| Grit with three context lines | 146.02 ms | 101,632 KiB | 100,800–101,988 KiB |

The fix reduces median wall time by 17%. Median peak RSS differs by 900 KiB (0.9%), with overlapping observed ranges. The fixed result matches system Git's patch-ID for a normal unified diff on this fixture.

## See also

- [Library maintenance — patch IDs](../../library/maintenance.md)
- [`grit pick`](pick.md)
