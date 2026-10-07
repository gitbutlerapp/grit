---
title: grit update
summary: Update grit to the latest release.
group: Maintenance
order: 1
---

## Synopsis

```
grit update
```

## Description

Downloads the latest release of `grit` and installs it over the copy you're running. It does this by running the same install script as the one-line install on the [docs overview](../#install): `curl -fsSL https://grit-scm.com/install | sh` on macOS and Linux, which needs `sh` and `curl`, and `irm https://grit-scm.com/install.ps1 | iex` in PowerShell on Windows.

The installer prints its own progress. If you installed `grit` with `cargo install`, update it with `cargo install grit-cli` instead.

## Options

`grit update` takes no options beyond the [global ones](../#options-for-every-command).

## Examples

```
$ grit update
Updating grit (current: 0.5.0)
Install directory: /home/ada/.local/bin
...
```

## JSON output

```
{
  "updated": true,
  "version": "0.5.0"
}
```

`version` is the version that ran the update, not the one that was installed. With `--json`, the installer's output goes to stderr so stdout carries only the JSON.

## See also

[Docs overview](../)
