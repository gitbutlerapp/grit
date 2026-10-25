---
title: grit slice
summary: Name sets of paths to check out together, kept on the server's target branch.
group: Remotes
order: 10
---

## Synopsis

```text
grit slice list [--url <url>]
grit slice new <name> -p|--path <path>... [-d|--description <text>] [--url <url>]
grit slice edit <name> [--add <path>]... [--remove <path>]... [-d|--description <text>] [--url <url>]
grit slice delete <name> [--url <url>]
```

## Description

A slice names a set of paths so people check out the same part of a repository without listing it each time: `grit checkout <url> <slice>` checks out a slice's paths (into a directory named after the slice), and `--path` adds more.

Slices live in `.grit/slices.toml` on the server's target branch, so they're versioned with the code they describe and every checkout sees the same ones:

```toml
[slices.desktop]
description = "The desktop app and its UI package"
paths = ["apps/desktop", "packages/ui"]
```

Each `new`, `edit` or `delete` is one commit on the target branch, made by the server, by you: the author your own commits get (`GIT_AUTHOR_NAME` and `GIT_AUTHOR_EMAIL`, then `author.*` and `user.*` config, as [`grit commit`](../commit/) has it), or the signed-in user when none is set. The server cleans `<`, `>` and line breaks out of the name and email. The paths must exist on the target. Slice changes and [integrations](../integrate/) take turns on the server, so several at once all land, each on the target the one before left; each is checked again on the target it lands on (a name taken meanwhile is refused). When the server's repository has the target checked out in a worktree, the worktree moves with the commit, keeping uncommitted changes there; changes the commit would overwrite refuse it. Inside a [snapshot checkout](../checkout/), `grit slice` talks to the server the checkout came from; elsewhere, pass `--url`. Changing slices needs write access; listing them needs read access. A checkout made from a slice keeps its paths when the slice changes later.

## Options

| Option | Description |
| --- | --- |
| `list` | List the slices. |
| `new <name>` | Make a slice. Names use letters, digits, `-`, `_` and `.`. |
| `edit <name>` | Change a slice's paths or description. |
| `delete <name>` | Delete a slice. |
| `-p`, `--path <path>` | With `new`: a path in the slice (repeatable, at least one). |
| `--add <path>` | With `edit`: add a path (repeatable). |
| `--remove <path>` | With `edit`: remove a path (repeatable). |
| `-d`, `--description <text>` | With `new` or `edit`: what the slice is for. |
| `--url <url>` | The repository whose slices to use. Defaults to the checkout you are in. |

## Examples

```console
$ grit slice new code -p src/deep -d "The deep code"
✓ Added slice code on main: 7f51f16 → 0416f7e
  src/deep
$ grit slice edit code --add docs
✓ Changed slice code on main: 0416f7e → 2f44ddc
  docs, src/deep
$ grit slice list
code  docs, src/deep
      The deep code
$ grit checkout http://127.0.0.1:8080/demo.git code
```

## JSON output

Schema: [slice.list v1](/schemas/slice.list/v1.json) · [slice.new v1](/schemas/slice.new/v1.json) · [slice.edit v1](/schemas/slice.edit/v1.json) · [slice.delete v1](/schemas/slice.delete/v1.json)

`grit slice list`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The server repository. |
| `target` | string | The target branch the slices are on. |
| `head` | string | Where it is. |
| `slices` | array | Each slice's `name`, `description` (when set) and `paths`. |

<!-- schema: slice.list -->

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "target": "main",
  "head": "2f44ddc44831f89c598c98491de99dab2af647c8",
  "slices": [
    { "name": "code", "description": "The deep code", "paths": ["docs", "src/deep"] }
  ]
}
```

`grit slice new`, `edit` and `delete`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `new`, `edit` or `delete`. |
| `name` | string | The slice's name. |
| `slice` | object or omitted | The slice as it is now: `name`, `description` and `paths`. Omitted once deleted. |
| `target` | string | The target branch. |
| `previous` | string | Where it was. |
| `head` | string | Where it is: the server's commit changing the slices file (`previous` when nothing changed). |

<!-- schema: slice.new -->

```json
{
  "action": "new",
  "name": "code",
  "slice": { "name": "code", "description": "The deep code", "paths": ["src/deep"] },
  "target": "main",
  "previous": "7f51f16b2cb0e4d3c2b1a0f9e8d7c6b5a4f3e2d1",
  "head": "0416f7e1b2c3d4e5f60718293a4b5c6d7e8f9012"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [slice.list v1](/schemas/slice.list/v1.json) · [slice.new v1](/schemas/slice.new/v1.json) · [slice.edit v1](/schemas/slice.edit/v1.json) · [slice.delete v1](/schemas/slice.delete/v1.json)

```markdown
## Slices on `main`

| Slice | Paths | Description |
| --- | --- | --- |
| `code` | `docs`, `src/deep` | The deep code |
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | The slice exists (`new`) or doesn't (`edit`, `delete`), a path is not on the target, the name is invalid, uncommitted changes in the server's worktree block the commit, the server offers no snapshot checkouts, or no server was named outside a checkout. |

## See also

- [`grit checkout`](../checkout/), [`grit serve`](../serve/).
