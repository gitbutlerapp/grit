---
title: grit auth
summary: Sign in to grithub, GitHub or a grit serve host, or save a username and token for any HTTPS host, so pushes and fetches just work.
group: Remotes
order: 5
---

## Synopsis

```text
grit auth [--token <token>]
grit auth <url> [--token <token>]
grit auth <url> --username <user> --token <token>
grit auth logout [<url>] [--dry-run]
```

`grit login` is the same command.

## Description

Signs you in and saves the token, so that [`grit push`](../push/), [`grit fetch`](../fetch/) and [`grit clone`](../clone/) work with private repositories over HTTPS.

Without a URL, `grit auth` signs in to [grithub](https://grithub.dev), unless you run it inside a repository whose `origin` is on github.com: then it signs in to GitHub. grithub signs in like a [`grit serve` host](#signing-in-to-a-grit-serve-host): `grit` opens grithub's sign-in page in your browser, you sign in with Google or your email address and approve the code, and `grit` saves the new token. Set the `GRIT_DEFAULT_HOST` environment variable to sign in to another grit server by default. `grit auth https://github.com` always signs in to GitHub.

GitHub sign-in uses GitHub's device flow. It prints a web address and a short code. Open `https://github.com/login/device` in your browser, enter the code and approve access. `grit` waits until you do, then saves the token with your credential helper. Every request goes straight to github.com.

The token is saved with your credential helper. When no `credential.helper` is set, `grit` sets one in your global config first and tells you which:

- on macOS, `osxkeychain` (the keychain);
- on Linux, `libsecret` when it is installed;
- on Windows, its built-in one ([`grit manager`](../manager/));
- otherwise `store`, which keeps tokens in plain text in `~/.git-credentials`.

To use a different one, set it yourself with [`grit config`](../config/) before signing in:

```console
$ grit config --global credential.helper libsecret
```

The helper is shared with Git and asked the same way: `grit` sends it the protocol and host (`protocol=https`, `host=github.com`), so one stored entry covers every repository on that host, and entries Git already saved, such as lines in `~/.git-credentials`, work as they are. Set `credential.useHttpPath` to keep a separate entry per repository; the helper then also gets the repository path (`path=owner/repo.git`).

`grit auth logout` removes the saved token for the same default host from your credential helper: grithub's, or GitHub's inside a repository whose `origin` is on github.com.

`grit` signs in with its own GitHub OAuth app. To use a different one, set its client id in the `GRIT_GITHUB_CLIENT_ID` environment variable or the `grit.githubClientId` setting.

### Signing in to a `grit serve` host

`grit auth <url>` signs in to a server run by [`grit serve`](../serve/) instead. The URL can be the server's address or any repository URL on it, such as `http://git.example.com:8080/app.git`; without a scheme it is taken as `https://`. A `github.com` URL signs in to GitHub as above.

It works like the GitHub sign-in. `grit` prints the server's sign-in page and a short code, and opens the page in your browser when it can. Sign in there with your email address and password (from the server's admin), check that the page shows the same code, and approve. `grit` then saves a new token with your credential helper for the server's scheme and host, under your email address. After that, `grit` and `git` clone, fetch and push to every repository on the host without asking.

To store a token you already have instead, for example one an admin made with `grit serve token` for a CI job, pass `--token`: `grit` checks it with the server before saving it.

`grit auth logout <url>` removes the token saved for that host.

Any other host, such as GitLab, Gitea or a company server, is not a `grit serve` host: `grit auth <url>` says so and points at `--username` and `--token` below, without changing your config.

### Saving a username and token for any HTTPS host

`grit` never asks for a password on the terminal, so pushes and fetches don't hang waiting for input. To save the user name and personal access token (or password) a host gave you, pass both:

```console
$ grit auth https://gitlab.com/team/app.git --username ada --token "$GITLAB_TOKEN"
✓ Stored the token for ada on gitlab.com — the next push or fetch uses it.
```

This works for any `http://` or `https://` URL, GitHub included. `grit` saves the pair with your credential helper (setting one up first, as above) under the URL's scheme and host, so it covers every repository on that host, for Git as well. With `credential.useHttpPath` set, it is saved for that repository's path instead, matching how pushes and fetches look it up, so pass the repository URL; `credential.<url>.helper` settings for the URL apply too. Nothing is sent to the host at this point: the next push, fetch or clone uses the token, and if the host refuses it, that command fails with `kind` `auth_rejected` and the helper drops it, as Git does. Run the same command again with the new token. A token on the command line can end up in your shell history; in CI, an askpass program (see [Credentials](../agents/#credentials-https)) avoids that.

## Options

| Option | Description |
| --- | --- |
| `<url>` | Sign in to the `grit serve` host this URL names instead of GitHub (or, with `--username`, the host to store a token for). |
| `--token <token>` | Store this access token: alone, for a `grit serve` host (from `grit serve token`), checked with the server instead of signing in in the browser; with `--username`, for any HTTP(S) host. A token on the command line can end up in your shell history. |
| `--username <user>` | Store `--token` for this user name on the URL's host, without contacting it. Needs `<url>` and `--token`. |
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

Sign in to a `grit serve` host in the browser, then clone without a password:

```console
$ grit login http://git.example.com:8080
To sign in, open this page in your browser:

    http://git.example.com:8080/.grit/authorize?code=K7MQ-3XZP

and check that it shows the code:

    K7MQ-3XZP

(Opened it for you.)
Waiting for you to approve… (press Ctrl-C to cancel)
✓ Signed in to git.example.com:8080 as bob@example.com — token stored.
$ grit clone http://git.example.com:8080/app.git
$ grit auth logout http://git.example.com:8080
✓ Signed out of git.example.com:8080 — removed the stored token for git.example.com:8080.
```

Store a token made with `grit serve token`, for example on a CI machine:

```console
$ grit auth http://git.example.com:8080 --token "$GRIT_TOKEN"
✓ Signed in to git.example.com:8080 as ci@example.com — token stored.
```

## JSON output

Schema: [v1](/schemas/auth/v1.json) · [auth.logout v1](/schemas/auth.logout/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `authenticated` | boolean | For sign-in, whether a token was stored. |
| `logged_out` | boolean | For `logout`, whether a stored token was removed (or would be removed with `--dry-run`). |
| `host` | string | Host the credential applies to: `github.com`, or a `grit serve` host with its port (`git.example.com:8080`). |
| `user` | string or omitted | For sign-in to a `grit serve` host, the email address of the user the token belongs to; with `--username`, that user name. Omitted for GitHub. |
| `verified` | boolean | For sign-in, whether the host confirmed the token. `false` with `--username`, which stores the token without contacting the host. |
| `dry_run` | boolean | On `logout`, `true` when `--dry-run` was used. |
| `credential` | object or omitted | With `--dry-run`, describes the helper entry that would be removed (`protocol`, `host`, `username`; never includes a password). |

After sign-in:

```json
{
  "authenticated": true,
  "host": "github.com",
  "verified": true
}
```

After signing in to a `grit serve` host:

```json
{
  "authenticated": true,
  "host": "git.example.com:8080",
  "user": "bob@example.com",
  "verified": true
}
```

After storing a token with `--username`:

```json
{
  "authenticated": true,
  "host": "gitlab.com",
  "user": "ada",
  "verified": false
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

For a `grit serve` host the line names the user: ``- Signed in to **`git.example.com:8080`** as **`bob@example.com`**``. With `--username` it reads ``- Stored a token for **`ada`** on **`gitlab.com`** (not verified; the next push or fetch uses it)``.

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit push](../push/), [grit config](../config/), [grit manager](../manager/), [grit serve](../serve/)