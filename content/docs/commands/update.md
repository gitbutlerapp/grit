---
title: grit update
summary: Update grit to the latest release.
group: Maintenance
order: 1
---

## Synopsis

```text
grit update
```

## Description

Downloads the latest release of `grit` and installs it over the copy you're running. It does this by running the same install script as the one-line install on the [Install](../install/) page: `curl -fsSL https://grit-scm.com/install | sh` on macOS and Linux, which needs `sh` and `curl`, and `irm https://grit-scm.com/install.ps1 | iex` in PowerShell on Windows.

The installer prints its own progress. If you installed `grit` with `cargo install`, update it with `cargo install grit-cli` instead.

## Options

`grit update` takes no options beyond the [global ones](../global-options/).

## Examples

```console
$ grit update
Updating grit (current: 0.5.0)
Install directory: /home/ada/.local/bin
...
```

## JSON output

Schema: [v1](/schemas/update/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `updated` | boolean | Whether the install script was started successfully. |
| `version` | string | Version of `grit` that ran the update (not necessarily the version installed). |

Example:

```json
{
  "updated": true,
  "version": "0.5.0"
}
```

With `--json`, the installer's progress goes to stderr so stdout carries only the JSON object.

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/update/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.



```text
## Update

- Updated grit (ran installer; previous version **[VERSION]**)
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[Docs overview](../)