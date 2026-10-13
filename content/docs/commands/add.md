---
title: grit add
summary: Stage changes. With no paths, stages everything.
group: Making changes
order: 1
---

## Synopsis

```text
grit add [<path>...]
```

## Description

Stages changes so they show up under **Staged** in [`grit status`](../status/). With no paths, `grit add` stages everything `grit status` reports: modified files, deleted files and untracked files. With paths, it stages only those files, or everything under those directories.

Paths are relative to your current directory, so from inside `src/`, `grit add main.rs` stages `src/main.rs`. Paths like `../README.md` and absolute paths inside the repository work too. A path that matches nothing is an error, and nothing is staged.

[`grit commit`](../commit/) stages every change before it records a commit, so you don't need `grit add` to commit. It's useful for starting to track a new file and checking what will be committed.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to stage. Omit to stage every change. |

## Examples

Stage everything:

```console
$ grit add
Staged 3 changes.
```

Stage one file and a directory:

```console
$ grit add README.md src/
Staged 2 changes.
```

## JSON output

Schema: [v1](/schemas/add/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `staged` | number | How many paths were staged by this invocation. |

Example:

```json
{
  "staged": 2
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for an `Add` heading and how many paths were staged:

```text
- Staged **1** change
```

When there is nothing to stage and you ran `grit add` with no paths:

```text
Nothing to stage — working tree clean.
```

## See also

[grit status](../status/), [grit commit](../commit/)
