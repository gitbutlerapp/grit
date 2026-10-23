---
title: grit remote
summary: List remotes, add one, or list refs on a remote.
group: Remotes
order: 1
---

## Synopsis

```text
grit remote [-v]
grit remote add <name> <url>
grit remote set-url <name> <url> [--push] [--dry-run]
grit remote refs <remote-or-url> [--heads] [--tags] [--sort name|version] [--reverse] [-n N] [<prefix>…]
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs, sorted by name. Like `git remote`, the list includes a remote that has config but no URL, shown as `(no url)`.

`grit remote add` adds a remote. It writes `remote.<name>.url` and the fetch refspec `+refs/heads/*:refs/remotes/<name>/*` to the repository's shared config, also when run from a linked worktree. It fails if a remote with that name is already configured, or if the name could not be used in a ref such as `refs/remotes/<name>/main` (for example names with spaces, `..` or `:`). [`grit fetch`](../fetch/) then stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](../clone/) adds a remote called `origin` for you.

`-v` / `--verbose` lists fetch and push URLs on separate lines (`name url (fetch)` and `name url (push)`), like `git remote -v`. With `--json`, each remote also includes `fetch_url` and `push_url` after `url.*.insteadOf` / `pushInsteadOf` rewrites, plus `refspecs`.

`grit remote set-url` changes the fetch URL (`remote.<name>.url`) or, with `--push`, replaces every `remote.<name>.pushurl` with a single new push URL. It fails when the remote is not configured. `--dry-run` reports the change without writing config.

`grit remote refs` lists references on a configured remote name or a literal URL, scp-style `host:path` or path (same transports as [`grit clone`](../clone/)), matching `git ls-remote` ordering. An argument that is none of these fails with `unknown remote`. SSH URLs use the same `GIT_SSH_COMMAND`, `GIT_SSH`, and `core.sshCommand` resolution as [`grit clone`](../clone/) and [`grit fetch`](../fetch/).

## Options

| Option | Description |
| --- | --- |
| `-v`, `--verbose` | With no subcommand, show fetch and push URLs (like `git remote -v`). |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists or the name is not valid in a ref. |
| `set-url <name> <url>` | Set the fetch URL for `<name>`. Use `--push` for `pushurl`. |
| `--push` | With `set-url`, update push URL(s) instead of fetch. |
| `--dry-run` | With `set-url`, show the change without writing config. |
| `refs <remote-or-url>` | List refs on the remote (or URL). |
| `--heads` | With `refs`, only `refs/heads/`. |
| `--tags` | With `refs`, only `refs/tags/`. |
| `--sort` | With `refs`, `name` (default) or `version` (`git ls-remote --sort=version:refname`). |
| `--reverse` | With `refs`, reverse `--sort` order. |
| `-n`, `--limit` | With `refs`, return at most `N` lines after sorting. |
| `<prefix>…` | With `refs`, optional ref prefixes (same rules as `git ls-remote`). |

## Examples

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git

$ grit remote -v
origin	https://github.com/ada/project.git (fetch)
origin	https://github.com/ada/project.git (push)

$ grit remote set-url origin git@github.com:ada/project.git
Set remote origin fetch URL to git@github.com:ada/project.git

$ grit remote refs origin
a1b2c3d4e5f6789012345678901234567890abcd	refs/heads/main
```

## JSON output

Schema: [v1](/schemas/remote/v1.json) · [remote.add v1](/schemas/remote.add/v1.json) · [remote.refs v1](/schemas/remote.refs/v1.json) (set-url uses the main remote schema)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, `set_url`, or `refs`. |
| `remotes` | array | For `list`, each remote with `name` and `url`; with `-v`, also `fetch_url`, `push_url`, and `refspecs`. |
| `name` | string | For `add` or `set_url`, the remote name. |
| `url` | string | For `add` or `set_url`, the remote URL or path. |
| `kind` | string | For `set_url`, `fetch` or `push`. |
| `old_url` | string or null | For `set_url`, previous URL for that kind, if any. |
| `dry_run` | boolean | For `set_url`, whether config was left unchanged. |
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

Setting a URL:

```json
{
  "action": "set_url",
  "name": "origin",
  "kind": "fetch",
  "old_url": "https://github.com/ada/old.git",
  "url": "https://github.com/ada/project.git",
  "dry_run": false
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

Schema: [v1](/schemas/remote/v1.json)
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
