---
title: grit config
summary: Read, set, list or remove configuration values.
group: Getting started
order: 4
---

## Synopsis

```text
grit config [<scope>] <key>
grit config [<scope>] <key> <value>
grit config [<scope>] --unset <key>
grit config [<scope>] --unset-all <key>
grit config [<scope>] --add <key> <value>
grit config [<scope>] --all <key>
grit config [<scope>] --list [<prefix>]
grit config [<scope>] --rename-section OLD --rename-section-new NEW
grit config [<scope>] --remove-section NAME
```

`<scope>` is one of `--global`, `--local`, `--system`, `--worktree`, or `-f <path>` (mutually exclusive).

## Description

Reads and writes Git configuration. `grit` uses the same config files as Git: the repository's `.git/config`, and your per-user `~/.gitconfig`. Settings you've made with Git apply to `grit`, and the other way around.

With just a key, `grit config` prints its value. By default, reads use the full config cascade (system, global, local, then command-line overrides), with later layers winning. Writes go to the repository's local config (`.git/config`). Use `--global` or `-f` to read or write a different file instead. `--local` limits reads to the repository config only (writes already target that file).

Keys are written as `section.name`, for example `user.email`.

### Settings grit uses

| Key | Used for |
| --- | --- |
| `user.name`, `user.email` | The author and committer of new commits. |
| `target.branch` | The branch [`grit status`](../status/) and [`grit shortlog`](../shortlog/) compare against. |
| `branch.<name>.remote` | Which remote [`grit push`](../push/), [`grit fetch`](../fetch/) and [`grit pull`](../pull/) use for a branch. Defaults to `origin`. |
| `branch.<name>.merge` | Which remote branch a branch pushes to and pulls from. Defaults to the same name. |
| `credential.helper` | Where HTTPS credentials are stored and looked up. See [`grit auth`](../auth/). |
| `grit.githubClientId` | The GitHub OAuth app [`grit auth`](../auth/) signs in with. |
| `grit.changeIds` | When true (default), [`grit commit`](../commit/) adds a `change-id` header to new commits and stores derived ids on amends when the old tip had none. |
| `grit.testing.changeId` | Test-only: `content-hash` makes change ids deterministic from commit bytes; any other value is used verbatim. |
| `receive.denyNonFastForwards` | Refuse pushes that would discard commits. Enforced on the remote for [`grit receive-pack`](../receive-pack/), smart HTTP, and local-path [`grit push`](../push/). |
| `receive.denyDeletes` | Refuse pushes that delete branches or tags. Same enforcement as `receive.denyNonFastForwards`. |
| `receive.denyCurrentBranch` | Refuse pushes to the branch checked out in a non-bare repository. On by default; same enforcement as the other `receive.deny*` keys. |

## Options

| Option | Description |
| --- | --- |
| `<key>` | The setting to read, set or remove. |
| `<value>` | The value to set. Omit it to read the current value. |
| `--global` | Use your per-user config instead of the repository's. |
| `--local` | Read or write only this repository's config file. Default for writes; optional for reads. |
| `--system` | Read or write the system config file (`GIT_CONFIG_SYSTEM` or `/etc/gitconfig`). |
| `--worktree` | Read or write `config.worktree` (requires `extensions.worktreeConfig` in the common config). |
| `-f`, `--file <path>` | Read or write only the given config file (no repository required). |
| `-l`, `--list` | List settings as `key=value`. Optional `<prefix>` limits to canonical keys with that prefix. |
| `--all` | Print every value for a multi-valued key (load order). |
| `--add` | Append a value without replacing existing entries. |
| `--unset` | Remove the last matching value for a key. |
| `--unset-all` | Remove every value for a key. |
| `--type bool\|int\|path` | Parse or canonicalize values like `git config --type`. |
| `--default <value>` | When reading, use this value if the key is unset (before `--type` coercion). |
| `--rename-section OLD` | Rename a section; requires `--rename-section-new NEW`. |
| `--rename-section-new NEW` | New section name for `--rename-section`. |
| `--remove-section NAME` | Delete a section and all of its keys. |
| `--dry-run` | On writes, report what would change without modifying the config file. |

### Exit codes

| Situation | Code |
| --- | --- |
| Success | `0` |
| `get` when the key is not set | `1` (human mode prints nothing; `--json` still prints a `get` object with `"value": null`) |
| Other errors (invalid key, not in a repository for `--local`, …) | `1` with an error message |

Reading or removing a key that is not set in the chosen scope is an error for `--unset`; `get` treats a missing key as exit `1` without treating it as a command failure in `--json` mode.

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

Read only the repository file (ignore global overrides for keys set locally):

```console
$ grit config --local user.email
ada@work.example
```

Edit a standalone config file:

```console
$ grit config -f ~/.gitconfig.user user.email ada@personal.example
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

Schema: [v2](/schemas/config/v2.json) · [v1](/schemas/config/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `get`, `get_all`, `set`, `add`, `unset`, `unset_all`, `list`, `rename_section`, or `remove_section`. |
| `key` | string | Configuration key for `get`, `set`, or `unset`. |
| `value` | string or `null` | Value for `get` or `set`. `null` when `get` did not find the key (exit `1`). |
| `scope` | string or `null` | For `get`, which layer won (`system`, `global`, `local`, `worktree`, `command`). |
| `file` | string or `null` | For `get`, source path when file-backed. |
| `line` | integer or `null` | For `get`, one-based line number in `file`. |
| `entries` | array | For `list`, each entry with `key`, optional `value`, `scope`, and optional `file`. |
| `values` | array | For `get_all`, each `{ value, scope, file?, line? }`. |
| `dry_run` | boolean | Present on write actions when `--dry-run` was used. |
| `preview` | string | Human summary of the pending change when `dry_run` is true. |
| `removed` | integer | For `unset_all`, number of values removed. |
| `old_name` | string | For `rename_section`, the previous section name. |
| `new_name` | string | For `rename_section`, the new section name. |
| `section` | string | For `remove_section`, the section that was removed. |

Reading a value:

```json
{
  "action": "get",
  "key": "user.name",
  "value": "Ada Lovelace",
  "scope": "global",
  "file": "/home/ada/.gitconfig",
  "line": 2
}
```

Missing key:

```json
{
  "action": "get",
  "key": "missing.key",
  "value": null,
  "scope": null,
  "file": null,
  "line": null
}
```

Listing values:

```json
{
  "action": "list",
  "entries": [
    {
      "key": "user.name",
      "value": "Ada Lovelace",
      "scope": "global",
      "file": "/home/ada/.gitconfig"
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v2](/schemas/config/v2.json) · [v1](/schemas/config/v1.json)

For `get`, markdown includes the value (when set) plus scope, file, and line. See the JSON output section for the stable field contract.




```text
| Key | Value |
| --- | --- |
| `core.repositoryformatversion` | 0 |
| `core.filemode` | true |
| `core.bare` | false |
| `core.logallrefupdates` | true |
| `user.email` | snap@example.com |
| `gc.auto` | 0 |
| `maintenance.auto` | false |
| `grit.testing.changeid` | content-hash |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit auth](../auth/), [grit status](../status/)