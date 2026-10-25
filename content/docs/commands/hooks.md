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

Hooks run with the invoking environment (including `PATH`), from the top of the work tree. A script the OS will not execute directly runs through `sh`: `/bin/sh` on Unix, while on Windows a script's `#!` line names its interpreter (`#!/bin/sh` → `sh`, `#!/usr/bin/env bash` → `bash`), looked up first in the Git for Windows install that owns the `git` on `PATH` (`usr\bin`, `bin`), then on `PATH`, skipping the WSL launchers in `System32` and `WindowsApps`. Hook paths, in `list` output and on the hook's command line, never carry the Windows `\\?\` prefix.

When a hook cannot be started at all (for example, its interpreter is missing), the command fails with an error naming the hook, the program it tried to start and the OS error, instead of reporting an exit status:

```console
$ grit hooks run pre-commit
error: failed to run hook pre-commit (.git/hooks/pre-commit): No such file or directory (os error 2)
$ echo $?
1
```

In `--json` mode the same message is the `error` field. [`grit commit`](commit.md) reports it the same way for `pre-commit` and `commit-msg`; for `post-commit` and the other hooks that run after an operation completes, it is a warning and the operation still succeeds.

[`grit commit`](commit.md) runs `pre-commit` and `commit-msg` automatically before writing the commit object, then `post-commit` after the branch moves. Use `grit commit --no-verify` to skip the first two (same as Git). JSON commit output lists hook basenames that actually ran in `hooks_run`.

Other commands run the hooks Git runs after them, with Git's arguments:

| Hook | Run by | Arguments and stdin |
| --- | --- | --- |
| `post-checkout` | [`grit switch`](switch.md), [`grit rebase`](rebase.md) and [`grit history`](history.md) rewrites that change `HEAD`'s tree | `<old HEAD> <new HEAD> 1` (all-zero old id from an unborn branch) |
| `post-merge` | [`grit merge`](merge.md) and [`grit pull`](pull.md) after a fast-forward or merge commit (`0`) or a squash (`1`) | `0` or `1` |
| `post-rewrite` | `grit commit --amend` (`amend`), `grit rebase` and `grit history` rewrites (`rebase`) | `amend` or `rebase`; stdin has one `<old> <new>` line per rewritten commit |

These hooks run after the operation is complete, so their exit status never changes the result, and a hook that cannot be started is a `hook_not_run` warning. `--dry-run` and `--check` run no hooks. [`grit switch`](switch.md) and [`grit commit`](commit.md) list the hooks that ran in their JSON `hooks_run` field.

## Options

### `grit hooks list`

| `-n`, `--limit <N>` | List at most `N` hook scripts (`truncated` in JSON when more exist). |

Other [global options](../global-options.md) apply.

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
| `truncated` | boolean | For `list`: `true` when more hook scripts existed than returned (`-n` / `--limit`). |
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



```text
## Hooks

| Name | Path | Executable | Source |
| --- | --- | --- | --- |
| `pre-commit` | `[REPO]/.git/hooks/pre-commit` | true | hooks_dir |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | `hooks list` or `hooks run` when the hook exits 0 (or the hook is missing). |
| *varies* | `hooks run` returns the hook subprocess exit code ([`custom`](../exit-codes/)). |
| `1` | `hooks run` when a hook could not be started ([`failure`](../exit-codes/)). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

- [Library maintenance — hooks](../../library/maintenance.md)
- [`grit patch-id`](patch-id.md)
