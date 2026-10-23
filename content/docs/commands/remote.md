---
title: grit remote
summary: List remotes, add one, show one, or list refs on a remote.
group: Remotes
order: 1
---

## Synopsis

```text
grit remote
grit remote add <name> <url>
grit remote show <name> [--no-query] [-n N]
grit remote refs <remote-or-url> [--heads] [--tags] [--sort name|version] [--reverse] [-n N] [<prefix>…]
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs, sorted by name. Like `git remote`, the list includes a remote that has config but no URL, shown as `(no url)`.

`grit remote add` adds a remote. It writes `remote.<name>.url` and the fetch refspec `+refs/heads/*:refs/remotes/<name>/*` to the repository's shared config, also when run from a linked worktree. It fails if a remote with that name is already configured, or if the name could not be used in a ref such as `refs/remotes/<name>/main` (for example names with spaces, `..` or `:`). [`grit fetch`](../fetch/) then stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](../clone/) adds a remote called `origin` for you.

`grit remote show` summarizes one configured remote: fetch and push URLs, the remote `HEAD` branch, each remote branch classified as tracked, new, or stale (when the remote is queried), and local branches whose `branch.<name>.remote` points at this remote. By default it contacts the remote (same transports as [`grit fetch`](../fetch/)). With `--no-query`, branch rows come from local remote-tracking refs only and every branch is reported as tracked.

`grit remote refs` lists references on a configured remote name or a literal URL, scp-style `host:path` or path (same transports as [`grit clone`](../clone/)), matching `git ls-remote` ordering. An argument that is none of these fails with `unknown remote`. SSH URLs use the same `GIT_SSH_COMMAND`, `GIT_SSH`, and `core.sshCommand` resolution as [`grit clone`](../clone/) and [`grit fetch`](../fetch/).

## Options

| Option | Description |
| --- | --- |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists or the name is not valid in a ref. |
| `show <name>` | Summarize `<name>` (URLs, HEAD branch, remote branches, pull config). |
| `--no-query` | With `show`, use local remote-tracking refs only; do not contact the remote. |
| `-n`, `--limit` | With `show` or `refs`, return at most `N` branch or ref lines after sorting. |
| `refs <remote-or-url>` | List refs on the remote (or URL). |
| `--heads` | With `refs`, only `refs/heads/`. |
| `--tags` | With `refs`, only `refs/tags/`. |
| `--sort` | With `refs`, `name` (default) or `version` (`git ls-remote --sort=version:refname`). |
| `--reverse` | With `refs`, reverse `--sort` order. |
| `<prefix>…` | With `refs`, optional ref prefixes (same rules as `git ls-remote`). |

## Examples

```console
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git

$ grit remote show origin
* remote origin
  Fetch URL: https://github.com/ada/project.git
  Push  URL: https://github.com/ada/project.git
  HEAD branch: main
  Remote branches:
    main tracked
    feature new
  Local branches configured for pull:
    main merges with remote main

$ grit remote refs origin
a1b2c3d4e5f6789012345678901234567890abcd	refs/heads/main
```

## JSON output

Schema: [v1](/schemas/remote/v1.json) · [remote.add v1](/schemas/remote.add/v1.json) · [remote.show v1](/schemas/remote.show/v1.json) · [remote.refs v1](/schemas/remote.refs/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, `show`, or `refs`. |
| `remotes` | array | For `list`, each remote with `name` and `url`. |
| `name` | string | For `add` or `show`, the remote name. |
| `url` | string | For `add`, the remote URL or path. |
| `fetch_url` | string | For `show`, fetch URL after config rewrite. |
| `push_url` | string | For `show`, push URL (`pushurl` when set). |
| `head_branch` | string or null | For `show`, remote default branch when known. |
| `branches` | array | For `show`, each `{ name, state }` with `state` `tracked`, `new`, or `stale`. |
| `pull` | array | For `show`, each `{ local, merges }` from `branch.*` config. |
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

Showing a remote:

```json
{
  "action": "show",
  "name": "origin",
  "fetch_url": "https://github.com/ada/project.git",
  "push_url": "https://github.com/ada/project.git",
  "head_branch": "main",
  "branches": [
    { "name": "main", "state": "tracked" },
    { "name": "feature", "state": "new" }
  ],
  "pull": [
    { "local": "main", "merges": "refs/heads/main" }
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

Schema: [v1](/schemas/remote/v1.json)

Pass `--markdown` for agent-friendly tables. `show` prints a summary table plus branch and pull sections. `refs` prints a “Remote refs” table (not JSON). Example shape for `show`:

```text
## Remote `origin`

| | |
| --- | --- |
| Fetch URL | https://github.com/ada/project.git |
| Push URL | https://github.com/ada/project.git |
| HEAD branch | `main` |

### Remote branches

| Branch | State |
| --- | --- |
| `main` | tracked |
| `feature` | new |
```

## See also

[grit fetch](../fetch/), [grit push](../push/), [grit clone](../clone/)
