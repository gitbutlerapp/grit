---
title: grit manager
summary: A credential helper that stores passwords and tokens in the Windows Credential Manager.
group: Plumbing
order: 3
---

## Synopsis

```
grit manager (get | store | erase)
```

## Description

A Git credential helper backed by the Windows Credential Manager. You don't run it yourself: Git and `grit` run it when they need a password or token. It speaks Git's [credential helper protocol](https://git-scm.com/docs/gitcredentials), reading the request on stdin and, for `get`, writing the credentials it found on stdout.

[`grit auth`](../auth/) sets `credential.helper` to `grit manager` on Windows when no other helper is configured. To set it up yourself:

```
> grit config --global credential.helper "grit manager"
```

It works only on Windows. On other systems it exits with an error.

## Options

| Option | Description |
| --- | --- |
| `get` | Look up stored credentials for the request. |
| `store` | Save the credentials in the request. |
| `erase` | Remove the credentials matching the request. |

## Examples

```
> echo "protocol=https`nhost=github.com`n" | grit manager get
protocol=https
host=github.com
username=x-access-token
password=gho_...
```

## JSON output

None. `grit manager` always speaks the credential helper protocol.

## See also

[grit auth](../auth/), [grit config](../config/)
