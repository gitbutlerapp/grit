---
title: grit auth
summary: Sign in to GitHub or a grit serve host so pushes and fetches just work.
group: Remotes
order: 5
---

## Synopsis

```text
grit auth
grit auth <url> [--token <token>]
grit auth logout [<url>] [--dry-run]
```

## Description

Signs you in to GitHub and saves the token, so that [`grit push`](../push/), [`grit fetch`](../fetch/) and [`grit clone`](../clone/) work with private repositories over HTTPS.

`grit auth` uses GitHub's device flow. It prints a web address and a short code. Open `https://github.com/login/device` in your browser, enter the code and approve access. `grit` waits until you do, then saves the token with your credential helper. Every request goes straight to github.com.

A credential helper has to be configured to store the token. On Windows, if none is set, `grit` uses its built-in one ([`grit manager`](../manager/)). Elsewhere, set one with [`grit config`](../config/):

```console
$ grit config --global credential.helper osxkeychain   # macOS
$ grit config --global credential.helper libsecret     # Linux
$ grit config --global credential.helper store         # a plain-text file, any system
```

The helper is shared with Git and asked the same way: `grit` sends it the protocol and host (`protocol=https`, `host=github.com`), so one stored entry covers every repository on that host, and entries Git already saved, such as lines in `~/.git-credentials`, work as they are. Set `credential.useHttpPath` to keep a separate entry per repository; the helper then also gets the repository path (`path=owner/repo.git`).

`grit auth logout` removes the saved GitHub token from your credential helper.

`grit` signs in with its own GitHub OAuth app. To use a different one, set its client id in the `GRIT_GITHUB_CLIENT_ID` environment variable or the `grit.githubClientId` setting.

### Signing in to a `grit serve` host

`grit auth <url>` signs in to a server run by [`grit serve`](../serve/) instead. The URL can be the server's address or any repository URL on it, such as `http://git.example.com:8080/app.git`; without a scheme it is taken as `https://`. A `github.com` URL signs in to GitHub as above.

A `grit serve` host has no browser sign-in: its admin creates your token with `grit serve add-user`. `grit auth <url>` asks for the token without echoing it (or reads it from stdin, or takes `--token`), checks it with the server, and saves it with your credential helper for the server's scheme and host, under your user name on the server. After that, `grit` and `git` clone, fetch and push to every repository on the host without the token in the URL.

`grit auth logout <url>` removes the token saved for that host.

## Options

| Option | Description |
| --- | --- |
| `<url>` | Sign in to the `grit serve` host this URL names instead of GitHub. |
| `--token <token>` | The access token for a `grit serve` host. Without it, `grit auth <url>` prompts for the token on a terminal and reads it from stdin otherwise. A token on the command line can end up in your shell history. |
| `logout` | Remove the saved GitHub token. |
| `logout <url>` | Remove the token saved for a `grit serve` host. |
| `--dry-run` | On `logout`, report which stored credential would be erased without calling the helper. |

## Examples

```console
$ grit auth
To authorize grit, open this page in your browser:

    https://github.com/login/device

and enter the code:

    1A2B-3C4D

Waiting for you to authorize… (press Ctrl-C to cancel)

✓ Signed in to GitHub — token stored for github.com.

$ grit auth logout
✓ Signed out of GitHub — removed the stored token for github.com.
```

Sign in to a `grit serve` host with the token its admin gave you, then clone without it:

```console
$ grit auth http://git.example.com:8080
Token (from `grit serve add-user`):
✓ Signed in to git.example.com:8080 as bob — token stored.
$ grit clone http://git.example.com:8080/app.git
$ grit auth logout http://git.example.com:8080
✓ Signed out of git.example.com:8080 — removed the stored token for git.example.com:8080.
```

## JSON output

Schema: [v1](/schemas/auth/v1.json) · [auth.logout v1](/schemas/auth.logout/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `authenticated` | boolean | For sign-in, whether a token was stored. |
| `logged_out` | boolean | For `logout`, whether a stored token was removed (or would be removed with `--dry-run`). |
| `host` | string | Host the credential applies to: `github.com`, or a `grit serve` host with its port (`git.example.com:8080`). |
| `user` | string or omitted | For sign-in to a `grit serve` host, the user the token belongs to. Omitted for GitHub. |
| `dry_run` | boolean | On `logout`, `true` when `--dry-run` was used. |
| `credential` | object or omitted | With `--dry-run`, describes the helper entry that would be removed (`protocol`, `host`, `username`; never includes a password). |

After sign-in:

```json
{
  "authenticated": true,
  "host": "github.com"
}
```

After signing in to a `grit serve` host:

```json
{
  "authenticated": true,
  "host": "git.example.com:8080",
  "user": "bob"
}
```

After logout:

```json
{
  "logged_out": true,
  "host": "github.com"
}
```

`logged_out` is `false` when no credential helper is configured, since there's nowhere a token could be stored.

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/auth/v1.json) · [auth.logout v1](/schemas/auth.logout/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.



```text
## Auth

- Signed in to **`github.com`**
```

For a `grit serve` host the line names the user: ``- Signed in to **`git.example.com:8080`** as **`bob`**``.

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit push](../push/), [grit config](../config/), [grit manager](../manager/), [grit serve](../serve/)