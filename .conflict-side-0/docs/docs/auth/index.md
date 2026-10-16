# grit auth

> Sign in to GitHub so HTTPS pushes and fetches just work.

## Synopsis

```text
grit auth
grit auth logout
```

## Description

Signs you in to GitHub and saves the token, so that [`grit push`](https://grit-scm.com/docs/push/index.md), [`grit fetch`](https://grit-scm.com/docs/fetch/index.md) and [`grit clone`](https://grit-scm.com/docs/clone/index.md) work with private repositories over HTTPS.

`grit auth` uses GitHub's device flow. It prints a web address and a short code. Open `https://github.com/login/device` in your browser, enter the code and approve access. `grit` waits until you do, then saves the token with your credential helper. Every request goes straight to github.com.

A credential helper has to be configured to store the token. On Windows, if none is set, `grit` uses its built-in one ([`grit manager`](https://grit-scm.com/docs/manager/index.md)). Elsewhere, set one with [`grit config`](https://grit-scm.com/docs/config/index.md):

```console
$ grit config --global credential.helper osxkeychain   # macOS
$ grit config --global credential.helper libsecret     # Linux
$ grit config --global credential.helper store         # a plain-text file, any system
```

`grit auth logout` removes the saved GitHub token from your credential helper.

`grit` signs in with its own GitHub OAuth app. To use a different one, set its client id in the `GRIT_GITHUB_CLIENT_ID` environment variable or the `grit.githubClientId` setting.

## Options

| Option | Description |
| --- | --- |
| `logout` | Remove the saved GitHub token. |

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

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `authenticated` | boolean | For sign-in, whether a token was stored. |
| `logged_out` | boolean | For `logout`, whether a stored token was removed. |
| `host` | string | GitHub host the credential applies to (usually `github.com`). |

After sign-in:

```json
{
  "authenticated": true,
  "host": "github.com"
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

## See also

[grit push](https://grit-scm.com/docs/push/index.md), [grit config](https://grit-scm.com/docs/config/index.md), [grit manager](https://grit-scm.com/docs/manager/index.md)
