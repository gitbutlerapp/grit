---
title: grit init
summary: Create a new, empty repository.
group: Getting started
order: 1
---

## Synopsis

```text
grit init [-b|--initial-branch <name>] [--bare] [--object-format sha1|sha256]
          [--ref-format <files|reftable>] [--separate-git-dir <dir>] [<path>]
```

Global options [`--json`](../global-options/) and [`--markdown`](../global-options/) apply.

## Description

Creates an empty Git repository at `<path>`, or in the current directory when no path is given. The directory is created if it doesn't exist. A normal repository keeps its data in a `.git` directory next to your files. A bare repository has no working tree; the repository data is the directory itself, which is what you want for a repository that other people push to.

The first branch defaults to `main`, or to `init.defaultBranch` from your global Git config when you omit `-b`. It has no commits until you make one.

Use `--ref-format reftable` to initialize with Git's reftable ref backend (requires a reftable-capable Git for interoperability). The default is `files` (loose refs under `refs/`).

Use `--object-format sha256` for a SHA-256 object database (Git repository format version 1 with `extensions.objectformat = sha256`). The default is `sha1`.

Use `--separate-git-dir` to keep the git directory outside the work tree (a `gitdir:` gitfile in `.git` points at the real directory).

## Options

| Option | Description |
| --- | --- |
| `<path>` | Where to create the repository. Defaults to the current directory. |
| `-b`, `--initial-branch` | Initial branch name (overrides `init.defaultBranch`). |
| `--bare` | Create a bare repository, with no working tree. |
| `--object-format` | Object hash algorithm: `sha1` (default) or `sha256`. |
| `--ref-format` | Ref storage backend: `files` (default) or `reftable`. |
| `--separate-git-dir` | Place the git directory at this path instead of `<path>/.git`. |

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

Schema: [v1](/schemas/init/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `initialized` | boolean | `true` when a new repository was created. |
| `reinitialized` | boolean | `true` when an existing repository was reinitialized in place (omitted when `false`). |
| `path` | string | Repository directory (the `.git` directory for a normal repository). |
| `bare` | boolean | Whether the repository is bare. |
| `branch` | string | Initial branch name (actual `HEAD` branch after reinit). |
| `object_format` | string | Object hash algorithm (`sha1` or `sha256`). |
| `ref_format` | string | Ref storage backend (`files` or `reftable`). |
| `git_dir` | string | Separate git directory when `--separate-git-dir` was used (omitted when co-located). |

Example:

```json
{
  "initialized": true,
  "path": "/home/ada/project/.git",
  "bare": false,
  "branch": "main",
  "object_format": "sha1",
  "ref_format": "files"
}
```

## Markdown output

Schema: [v1](/schemas/init/v1.json)
Pass [`--markdown`](../global-options/) for a short prose summary:

```text
Initialized empty repository at `/home/ada/project/.git` (default branch `main`, ref-format `reftable`).
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit clone](../clone/), [grit remote](../remote/)