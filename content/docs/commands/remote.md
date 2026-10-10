---
title: grit remote
summary: List remotes, add one, or list refs on a remote.
group: Remotes
order: 1
---

## Synopsis

```text
grit remote
grit remote add <name> <url>
grit remote refs <remote-or-url> [--heads] [--tags] [<prefix>…]
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs.

`grit remote add` adds a remote. [`grit fetch`](../fetch/) then stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](../clone/) adds a remote called `origin` for you.

`grit remote refs` lists references on a configured remote name or a literal URL/path (same transports as [`grit clone`](../clone/)), matching `git ls-remote` ordering.

## Options

| Option | Description |
| --- | --- |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists. |
| `refs <remote-or-url>` | List refs on the remote (or URL). |
| `--heads` | With `refs`, only `refs/heads/`. |
| `--tags` | With `refs`, only `refs/tags/`. |
| `<prefix>…` | With `refs`, optional ref prefixes (same rules as `git ls-remote`). |

## Examples

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git

$ grit remote refs origin
a1b2c3d4e5f6789012345678901234567890abcd	refs/heads/main
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, or `refs`. |
| `remotes` | array | For `list`, each remote with `name` and `url`. |
| `name` | string | For `add`, the remote name. |
| `url` | string | For `add`, the remote URL or path. |
| `refs` | array | For `refs`, each entry with `name`, `oid`, optional `peeled`, optional `symref_target`. |

Listing remotes:

```json
{
  "action": "list",
  "remotes": [
    {
      "name": "origin",
      "url": "https://github.com/ada/project.git"
    }
  ]
}
```

Adding a remote:

```json
{
  "action": "add",
  "name": "origin",
  "url": "https://github.com/ada/project.git"
}
```

Listing refs:

```json
{
  "action": "refs",
  "refs": [
    {
      "name": "refs/heads/main",
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "peeled": null,
      "symref_target": null
    }
  ]
}
```

## Markdown output

Pass `--markdown` for an agent-friendly Markdown table of refs (a “Remote refs” heading, then columns — not JSON). Example shape:

```text
Remote refs

| Ref | Object | Peeled | Symref target |
| --- | --- | --- | --- |
| `refs/heads/main` | `a1b2c3…` | — | — |
| `refs/tags/v1` | `tagoid…` | `commitoid…` | — |
```

## See also

[grit fetch](../fetch/), [grit push](../push/), [grit clone](../clone/)
