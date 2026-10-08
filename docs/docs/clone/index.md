# grit clone

> Copy a remote repository into a new directory.

## Synopsis

```
grit clone <url> [<dir>]
```

## Description

Copies the repository at `<url>` into a new directory and checks out the remote's default branch. The remote is saved as `origin`, so [`grit fetch`](https://grit-scm.com/docs/fetch/index.md), [`grit pull`](https://grit-scm.com/docs/pull/index.md) and [`grit push`](https://grit-scm.com/docs/push/index.md) work with no further setup.

`<url>` can be an `https://` or `http://` URL, an `ssh://` URL or `user@host:path` address, a `git://` URL, a `file://` URL, or a path to a local repository. A local path is stored as an absolute path so that later fetches work from anywhere.

Without `<dir>`, the directory is named after the last part of the URL with any `.git` suffix removed, so `https://github.com/gitbutlerapp/grit.git` clones into `grit`. The destination must not exist or must be empty.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](https://grit-scm.com/docs/auth/index.md).

## Options

| Option | Description |
| --- | --- |
| `<url>` | The repository to clone: a URL or a local path. |
| `<dir>` | The directory to clone into. Defaults to the repository's name. |

## Examples

Clone a repository from GitHub:

```
$ grit clone https://github.com/gitbutlerapp/grit.git
Cloning into 'grit' ...
Cloned into 'grit' on branch main.
```

Clone into a directory with a different name:

```
$ grit clone git@github.com:gitbutlerapp/grit.git grit-src
```

Clone a local repository:

```
$ grit clone /srv/git/project.git
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The URL or path that was cloned. |
| `path` | string | Directory the clone was created in. |
| `branch` | string | Branch that was checked out. |

Example:

```json
{
  "url": "/srv/git/project.git",
  "path": "project",
  "branch": "main"
}
```

## See also

[grit init](https://grit-scm.com/docs/init/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md), [grit fetch](https://grit-scm.com/docs/fetch/index.md), [grit auth](https://grit-scm.com/docs/auth/index.md)
