# grit update

> Update grit to the latest release.

## Synopsis

```
grit update
```

## Description

Downloads the latest release of `grit` and installs it over the copy you're running. It does this by running the same install script as the one-line install on the [Install](https://grit-scm.com/docs/install/index.md) page: `curl -fsSL https://grit-scm.com/install | sh` on macOS and Linux, which needs `sh` and `curl`, and `irm https://grit-scm.com/install.ps1 | iex` in PowerShell on Windows.

The installer prints its own progress. If you installed `grit` with `cargo install`, update it with `cargo install grit-cli` instead.

## Options

`grit update` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

```
$ grit update
Updating grit (current: 0.5.0)
Install directory: /home/ada/.local/bin
...
```

## JSON output

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

## See also

[Docs overview](https://grit-scm.com/docs/index.md)
