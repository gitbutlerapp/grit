---
title: grit merge-base
summary: Find common ancestors or test whether one commit is an ancestor of another.
group: History
order: 4
---

## Synopsis

```text
grit merge-base <A> <B> [--all] [--is-ancestor]
```

## Description

Finds merge bases between two commits, or checks reachability. With no flags, prints one minimal merge base (same default as Git). `--all` lists every minimal base (needed for criss-cross histories). `--is-ancestor` exits `0` when `A` is an ancestor of `B`, `1` when it is not, and prints nothing in human mode.

## Options

| Option | Description |
| --- | --- |
| `<A>`, `<B>` | Two commits or refs to compare. |
| `--all` | List all minimal merge bases. |
| `--is-ancestor` | Exit status only: ancestor test (`A` vs `B`). |

## Examples

Default merge base:

```console
$ grit merge-base main feature
919c45f33de5e5c0bd05f8ffb089f697f4644976
```

All merge bases:

```console
$ grit merge-base --all topic-a topic-b
```

Ancestor check (exit code 0):

```console
$ grit merge-base --is-ancestor HEAD~1 HEAD
$ echo $?
0
```

## JSON output

Schema: [v1](/schemas/merge-base/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `merge_bases` | array of strings | Hex object ids of merge bases (empty when `--is-ancestor` is set). |
| `is_ancestor` | boolean or null | `true` or `false` with `--is-ancestor`; otherwise `null`. |

Example:

```json
{
  "merge_bases": ["919c45f33de5e5c0bd05f8ffb089f697f4644976"],
  "is_ancestor": null
}
```

Example (`--is-ancestor`):

```json
{
  "merge_bases": [],
  "is_ancestor": true
}
```

## Markdown output

Schema: [v1](/schemas/merge-base/v1.json)

Pass [`--markdown`](../global-options/) for a short heading and bullet list of merge bases, or a sentence for `--is-ancestor`.

## See also

[grit revs](../revs/), [grit log](../log/), [grit diff](../diff/)
