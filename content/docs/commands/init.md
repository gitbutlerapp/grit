---
title: grit init
summary: Create a new, empty repository.
group: Getting started
order: 1
---

## Synopsis

```
grit init [--bare] [<path>]
```

## Description

Creates an empty Git repository at `<path>`, or in the current directory when no path is given. The directory is created if it doesn't exist. A normal repository keeps its data in a `.git` directory next to your files. A bare repository has no working tree; the repository data is the directory itself, which is what you want for a repository that other people push to.

The first branch is `main`. It has no commits until you make one.

## Options

| Option | Description |
| --- | --- |
| `<path>` | Where to create the repository. Defaults to the current directory. |
| `--bare` | Create a bare repository, with no working tree. |

## Examples

Start a new project in a new directory:

```
$ grit init project
Initialized empty repository in /home/ada/project/.git
```

Turn the current directory into a repository:

```
$ grit init
```

Create a bare repository to use as a shared remote:

```
$ grit init --bare /srv/git/project.git
Initialized empty bare repository in /srv/git/project.git
```

## JSON output

```
{
  "initialized": true,
  "path": "/home/ada/project/.git",
  "bare": false,
  "branch": "main"
}
```

`path` is the repository directory (the `.git` directory for a normal repository) and `branch` is the initial branch.

## See also

[grit clone](../clone/), [grit remote](../remote/)
