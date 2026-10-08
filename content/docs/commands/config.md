---
title: grit config
summary: Read, set, list or remove configuration values.
group: Getting started
order: 4
---

## Synopsis

```text
grit config [--global] <key>
grit config [--global] <key> <value>
grit config [--global] --unset <key>
grit config [--global] --list
```

## Description

Reads and writes Git configuration. `grit` uses the same config files as Git: the repository's `.git/config`, and your per-user `~/.gitconfig`. Settings you've made with Git apply to `grit`, and the other way around.

With just a key, `grit config` prints its value. Reads look at every config file, with the repository's settings taking priority over your global ones. With a key and a value, it sets the value in the repository's config. Add `--global` to read or write your per-user config instead.

Keys are written as `section.name`, for example `user.email`.

### Settings grit uses

| Key | Used for |
| --- | --- |
| `user.name`, `user.email` | The author and committer of new commits. |
| `target.branch` | The branch [`grit status`](../status/) and [`grit shortlog`](../shortlog/) compare against. |
| `branch.<name>.remote` | Which remote [`grit push`](../push/) and [`grit pull`](../pull/) use for a branch. Defaults to `origin`. |
| `branch.<name>.merge` | Which remote branch a branch pushes to and pulls from. Defaults to the same name. |
| `credential.helper` | Where HTTPS credentials are stored and looked up. See [`grit auth`](../auth/). |
| `grit.githubClientId` | The GitHub OAuth app [`grit auth`](../auth/) signs in with. |
| `receive.denyNonFastForwards` | Refuse pushes that would discard commits. Enforced by [`grit receive-pack`](../receive-pack/). |
| `receive.denyDeletes` | Refuse pushes that delete branches or tags. |
| `receive.denyCurrentBranch` | Refuse pushes to the branch checked out in a non-bare repository. On by default. |

## Options

| Option | Description |
| --- | --- |
| `<key>` | The setting to read, set or remove. |
| `<value>` | The value to set. Omit it to read the current value. |
| `--global` | Use your per-user config (`~/.gitconfig`) instead of the repository's. |
| `-l`, `--list` | List every setting as `key=value`. |
| `--unset` | Remove the setting. |

Reading or removing a key that isn't set is an error.

## Examples

Set your identity for every repository:

```console
$ grit config --global user.name "Ada Lovelace"
$ grit config --global user.email ada@example.com
```

Read a value:

```console
$ grit config user.email
ada@example.com
```

Use a different email address in one repository:

```console
$ grit config user.email ada@work.example
```

List everything:

```console
$ grit config --list
user.name=Ada Lovelace
user.email=ada@example.com
core.repositoryformatversion=0
core.bare=false
remote.origin.url=https://github.com/ada/project.git
remote.origin.fetch=+refs/heads/*:refs/remotes/origin/*
```

Remove a setting:

```console
$ grit config --unset target.branch
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `get`, `set`, `unset`, or `list`. |
| `key` | string | Configuration key for `get`, `set`, or `unset`. |
| `value` | string | Value for `get` or `set`. |
| `entries` | array | For `list`, each entry with `key` and optional `value`. |

Reading a value:

```json
{
  "action": "get",
  "key": "user.name",
  "value": "Ada Lovelace"
}
```

Listing values:

```json
{
  "action": "list",
  "entries": [
    { "key": "user.name", "value": "Ada Lovelace" }
  ]
}
```

## See also

[grit auth](../auth/), [grit status](../status/)
