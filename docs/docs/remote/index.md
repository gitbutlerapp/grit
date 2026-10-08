# grit remote

> List remotes, or add one.

## Synopsis

```
grit remote
grit remote add <name> <url>
```

## Description

A remote is another copy of the repository that you fetch from and push to, usually on a server. With no arguments, `grit remote` lists your remotes and their URLs.

`grit remote add` adds a remote. [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) then stores the remote's branches as remote-tracking branches named `<name>/<branch>`, such as `origin/main`. [`grit clone`](https://grit-scm.com/docs/clone/index.md) adds a remote called `origin` for you.

The URL can be anything [`grit clone`](https://grit-scm.com/docs/clone/index.md) accepts: an HTTPS, SSH, `git://` or `file://` URL, or a local path.

## Options

| Option | Description |
| --- | --- |
| `add <name> <url>` | Add a remote called `<name>` at `<url>`. Fails if a remote with that name exists. |

## Examples

```
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git

$ grit remote
origin	https://github.com/ada/project.git
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list` or `add`. |
| `remotes` | array | For `list`, each remote with `name` and `url`. |
| `name` | string | For `add`, the remote name. |
| `url` | string | For `add`, the remote URL or path. |

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

## See also

[grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit push](https://grit-scm.com/docs/push/index.md), [grit clone](https://grit-scm.com/docs/clone/index.md)
