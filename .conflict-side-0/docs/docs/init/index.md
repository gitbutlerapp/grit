# grit init

> Create a new, empty repository.

## Synopsis

```text
grit init [--bare] [--ref-format <files|reftable>] [<path>]
```

Global options [`--json`](https://grit-scm.com/docs/global-options/index.md) and [`--markdown`](https://grit-scm.com/docs/global-options/index.md) apply.

## Description

Creates an empty Git repository at `<path>`, or in the current directory when no path is given. The directory is created if it doesn't exist. A normal repository keeps its data in a `.git` directory next to your files. A bare repository has no working tree; the repository data is the directory itself, which is what you want for a repository that other people push to.

The first branch is `main`. It has no commits until you make one.

Use `--ref-format reftable` to initialize with Git's reftable ref backend (requires a reftable-capable Git for interoperability). The default is `files` (loose refs under `refs/`).

## Options

| Option | Description |
| --- | --- |
| `<path>` | Where to create the repository. Defaults to the current directory. |
| `--bare` | Create a bare repository, with no working tree. |
| `--ref-format` | Ref storage backend: `files` (default) or `reftable`. |

## Examples

Start a new project in a new directory:

```console
$ grit init project
Initialized empty repository in /home/ada/project/.git (ref-format: files)
```

Turn the current directory into a repository:

```console
$ grit init
```

Create a bare repository to use as a shared remote:

```console
$ grit init --bare /srv/git/project.git
Initialized empty bare repository in /srv/git/project.git (ref-format: files)
```

Initialize with the reftable ref backend:

```console
$ grit init --ref-format reftable reftable-demo
Initialized empty repository in /home/ada/reftable-demo/.git (ref-format: reftable)
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `initialized` | boolean | Whether the repository was created successfully. |
| `path` | string | Repository directory (the `.git` directory for a normal repository). |
| `bare` | boolean | Whether the repository is bare. |
| `branch` | string | Initial branch name. |
| `ref_format` | string | Ref storage backend (`files` or `reftable`). |

Example:

```json
{
  "initialized": true,
  "path": "/home/ada/project/.git",
  "bare": false,
  "branch": "main",
  "ref_format": "files"
}
```

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for a short prose summary:

```text
Initialized empty repository at `/home/ada/project/.git` (default branch `main`, ref-format `reftable`).
```

## See also

[grit clone](https://grit-scm.com/docs/clone/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md)
