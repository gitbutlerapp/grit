---
title: grit clone
summary: Copy a remote repository into a new directory.
group: Getting started
order: 2
---

## Synopsis

```
grit clone <url> [<dir>]
```

## Description

Copies the repository at `<url>` into a new directory and checks out the remote's default branch. The remote is saved as `origin`, so [`grit fetch`](../fetch/), [`grit pull`](../pull/) and [`grit push`](../push/) work with no further setup.

`<url>` can be an `https://` or `http://` URL, an `ssh://` URL or `user@host:path` address, a `git://` URL, a `file://` URL, or a path to a local repository. A local path is stored as an absolute path so that later fetches work from anywhere.

Without `<dir>`, the directory is named after the last part of the URL with any `.git` suffix removed, so `https://github.com/gitbutlerapp/grit.git` clones into `grit`. The destination must not exist or must be empty.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](../auth/).

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

```
{
  "url": "https://github.com/gitbutlerapp/grit.git",
  "path": "grit",
  "branch": "main"
}
```

`branch` is the branch that was checked out.

## See also

[grit init](../init/), [grit remote](../remote/), [grit fetch](../fetch/), [grit auth](../auth/)
