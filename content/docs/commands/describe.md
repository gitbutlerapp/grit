---
title: grit describe
summary: Nearest reachable tag, exact commit distance, and abbreviated id for a revision.
group: History
order: 5
---

## Synopsis

```text
grit describe [REV] [--tags] [--match GLOB] [--first-parent] [--abbrev N]
```

## Description

Finds the nearest tag reachable from `REV` (default `HEAD`) and reports the exact number of commits in the range from the tag’s peeled commit to `REV`. Output uses the long form `tag-distance-gabbrev` (including `-0-` when `REV` is exactly the tag commit).

By default only annotated tags are considered; `--tags` includes lightweight tags. The distance is the exact `peeled..REV` count (what `grit revs --count` reports), which can differ from Git’s lazy `git describe` depth and from which tag Git picks when many tags are reachable.

## Options

| Option | Description |
| --- | --- |
| `REV` | Commit or ref to describe (default `HEAD`). |
| `--tags` | Include lightweight tags (default: annotated only). |
| `--match GLOB` | Only consider tags whose short name matches `GLOB`. |
| `--first-parent` | Count distance along first-parent chains only. |
| `--abbrev N` | Minimum abbreviated commit id length (default follows `core.abbrev`, extending on collision like Git). |

## Examples

```console
$ grit describe --tags
v1.2.0-3-g4a1b2c3
```

With `--json`:

```console
$ grit describe --tags --json
```

## JSON output

Schema: [v1](/schemas/describe/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `tag` | string | Full `refs/tags/…` name. |
| `tag_short` | string | Short tag name. |
| `tag_oid` | string | Object id at the tag ref. |
| `peeled` | string | Peeled commit oid. |
| `distance` | integer | Exact commits in `peeled..commit`. |
| `commit` | string | Described commit oid. |
| `abbrev` | string | Abbreviated commit oid. |
| `long_format` | string | `tag-distance-gabbrev` line. |

Example:

```json
{
  "tag": "refs/tags/v1.2.0",
  "tag_short": "v1.2.0",
  "tag_oid": "a1b2c3d4e5f6789012345678901234567890abcd",
  "peeled": "1111111111111111111111111111111111111111",
  "distance": 3,
  "commit": "2222222222222222222222222222222222222222",
  "abbrev": "2222222",
  "long_format": "v1.2.0-3-g2222222"
}
```

## Markdown output

Schema: [v1](/schemas/describe/v1.json)

Pass [`--markdown`](../global-options/) for a short heading and bullet list of the tag, distance, and abbrev.

```text
# Describe

- **Tag:** `v1.2.0`
- **Distance:** 3
- **Commit:** `2222222`
- **Long:** `v1.2.0-3-g2222222`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success. |
| `1` | No reachable tag, or other failure. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/).

## See also

- [`revs`](revs.md) — count commits in a range.
- [`tag`](tag.md) — list or create tags.
