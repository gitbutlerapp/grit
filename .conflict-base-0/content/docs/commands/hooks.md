---
title: grit hooks
summary: List hook scripts and run them by name.
group: Plumbing
order: 1
---

## Synopsis

```text
grit hooks
grit hooks list
grit hooks run <NAME> [-- ARGS...]
```

## Description

Discovers traditional hook scripts in the resolved hooks directory (`core.hooksPath` when set, otherwise `.git/hooks`). Configured `hook.<name>.*` aliases are resolved when a hook runs, not in the list.

`grit hooks run` executes every hook registered for the event (config hooks first, then the script in the hooks directory) through the repository [`CommandRunner`](../../library/maintenance.md). The process exits with the hook's status when a hook ran and returned non-zero.

## Options

### `grit hooks list`

No flags beyond [global options](../global-options.md).

### `grit hooks run`

| Argument | Description |
| --- | --- |
| `NAME` | Hook basename (e.g. `pre-commit`). |
| `ARGS` | Optional arguments after `--`, forwarded to the hook. |

## Examples

List installed scripts:

```console
$ grit hooks list
pre-commit  /path/to/repo/.git/hooks/pre-commit  executable=yes  hooks_dir
```

Run a hook:

```console
$ grit hooks run pre-commit
$ echo $?
0
```

## JSON output

Schema: [v1](/schemas/hooks/v1.json). Subcommands: [hooks.list](/schemas/hooks.list/v1.json), [hooks.run](/schemas/hooks.run/v1.json).

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list` or `run`. |
| `hooks` | array | For `list`: each entry has `name`, `path`, `executable`, and `source` (`hooks_dir` or `core_hooks_path`). |
| `name` | string | For `run`: hook basename. |
| `exit_code` | number | For `run`: subprocess exit code (`0` when the hook was missing). |
| `executed` | boolean | For `run`: whether any hook script ran. |

Example list:

```json
{
  "action": "list",
  "hooks": [
    {
      "name": "pre-commit",
      "path": "/tmp/repo/.git/hooks/pre-commit",
      "executable": true,
      "source": "hooks_dir"
    }
  ]
}
```

Example run:

```json
{
  "action": "run",
  "name": "pre-commit",
  "exit_code": 0,
  "executed": true
}
```

## Markdown output

Schema: [v1](/schemas/hooks/v1.json)

`--markdown` renders a table of hooks for `list`, or a short summary for `run`.

## See also

- [Library maintenance — hooks](../../library/maintenance.md)
- [`grit patch-id`](patch-id.md)
