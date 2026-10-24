---
title: grit serve
summary: Serve repositories over HTTP, with a web view and token authentication.
group: Remotes
order: 7
---

## Synopsis

```text
grit serve [--root <path>] [--db <file>] [--bind <host:port>]
grit serve --setup [--admin-name <name>] [--root <path>] [--db <file>]
grit serve add-user <name> [--admin] [--root <path>] [--db <file>]
grit serve grant <user> <repository> [--write] [--root <path>] [--db <file>]
grit serve users [--root <path>] [--db <file>]
```

## Description

`grit serve` runs a Git server over HTTP. Any Git client, `git` or `grit`, can clone, fetch and push through it, and a browser shows each repository's branches and recent commits. Fetch speaks Git protocol v0, v1 and v2 (whichever the client asks for); push uses receive-pack, which has no v2 form. Everything runs in-process: the server needs no `git` or other `grit` binary.

`--root` names what to serve:

- a **directory of repositories**: each child directory that is a repository (bare `project.git` or a working tree `project`) is served as `project`. New repositories show up without a restart.
- a **single repository**: it is the only one served, under its directory name.

Clients reach a repository at `http://<host>:<port>/<name>.git` (the `.git` is optional).

### Users and tokens

Every request needs an access token. Users, tokens and repository grants live in a SQLite database, `grit-serve.db` in the root by default (or in the root's `.git` directory when the root is a working tree, so the file never shows up as untracked). Pick another path with `--db`.

- `grit serve --setup` creates the database and the first **admin** user, and prints the admin's token. An admin can read and write every repository.
- `grit serve add-user <name>` adds a user and prints their token. Add `--admin` for another admin.
- `grit serve grant <user> <repository>` gives a user read access (fetch and clone) to one repository; `--write` also allows pushing. Granting again replaces the earlier access.
- `grit serve users` lists every user, their token count and their access.

Each token is printed once, when it is created; the database keeps only a SHA-256 hash of it. A token starts with `grit_`.

Git sends the token as the password of HTTP basic auth. The easiest way to use one is [`grit auth <url>`](../auth/): it checks the token with the server and saves it with your credential helper, so `grit` and `git` clone, fetch and push without asking. A URL works too: `git clone http://alice:<token>@host:8080/project.git` (the user name in the URL is ignored). Other clients can send `Authorization: Bearer <token>`. In a browser, open the server's address and enter any user name with the token as the password.

A request without a valid token gets `401` and a prompt for credentials. A repository the user cannot read answers `404`, the same as one that does not exist, and a push from a user with read access answers `403`.

### Running it

`grit serve` with no subcommand and no `--setup` runs the server until interrupted. It refuses to start until `--setup` has created a user. It listens on `127.0.0.1:8080`; use `--bind 0.0.0.0:8080` to accept connections from other machines. The server speaks plain HTTP, so put it behind a TLS-terminating proxy before sending tokens across a network you don't trust.

## Options

| Option | Description |
| --- | --- |
| `--root <path>` | A repository, or a directory of repositories, to serve. Defaults to the current directory. Also accepted after a subcommand. |
| `--db <file>` | The user database. Defaults to `grit-serve.db` in the root (in its `.git` directory for a working tree). Also accepted after a subcommand. |
| `--bind <host:port>` | Address to listen on. Defaults to `127.0.0.1:8080`. |
| `--setup` | Create the database and its first admin user, print the admin's token, and exit. Fails when the database already has users. |
| `--admin-name <name>` | Name of the admin user `--setup` creates. Defaults to `admin`. Requires `--setup`. |
| `add-user <name>` | Add a user and print their token. User names are letters, digits, `.`, `_` or `-`. |
| `add-user --admin` | Make the new user an admin. |
| `grant <user> <repository>` | Give a user read access to a repository, by name with or without `.git`. |
| `grant --write` | Give write (push) access instead of read access. |
| `users` | List users, their tokens and their access. |

## Examples

Set up a directory of repositories and run the server:

```console
$ cd /srv/git
$ grit serve --setup
✓ Created admin user admin in grit-serve.db

  Token: grit_b3f1c87f0285256919e175ae157a5dee14ccc73f

Save the token now; it is not shown again. Use it as the password when Git asks.
$ grit serve
Serving 1 repository from /srv/git at http://127.0.0.1:8080/
  http://127.0.0.1:8080/demo.git
```

Sign in as the admin from another terminal, then clone and push:

```console
$ grit auth http://127.0.0.1:8080 --token grit_b3f1c87f0285256919e175ae157a5dee14ccc73f
✓ Signed in to 127.0.0.1:8080 as admin — token stored.
$ grit clone http://127.0.0.1:8080/demo.git
```

Add a user who can only fetch `demo`:

```console
$ grit serve add-user bob
✓ Created user bob in grit-serve.db

  Token: grit_5ba6de9a19b76e073acd3f327a46d39cf3f854c2

Save the token now; it is not shown again. Use it as the password when Git asks.
$ grit serve grant bob demo
✓ bob now has read access to demo
$ grit serve users
admin  1 token    all repositories (admin)
bob    1 token    demo (read)
```

A push from `bob` is refused:

```console
$ git push origin main
remote: bob has read-only access to demo
fatal: unable to access 'http://127.0.0.1:8080/demo/': The requested URL returned error: 403
```

Serve a single repository from its working tree:

```console
$ grit serve --root ~/projects/app --setup
$ grit serve --root ~/projects/app --bind 0.0.0.0:8080
```

## JSON output

Schema: [v1](/schemas/serve/v1.json) · [serve.add-user v1](/schemas/serve.add-user/v1.json) · [serve.grant v1](/schemas/serve.grant/v1.json) · [serve.users v1](/schemas/serve.users/v1.json)

Pass `--json` with `--setup` or a subcommand for one JSON object on stdout. The running server has no JSON output: it logs to stderr.

`grit serve --setup` and `grit serve add-user`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `database` | string | The user database the user was added to. |
| `user` | string | The new user's name. |
| `admin` | boolean | Whether the user is an admin (read and write on every repository). |
| `token` | string | The user's access token. It is shown only once. |

<!-- schema: serve.add-user -->

```json
{
  "database": "grit-serve.db",
  "user": "bob",
  "admin": false,
  "token": "grit_5ba6de9a19b76e073acd3f327a46d39cf3f854c2"
}
```

`grit serve grant`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `user` | string | The user given access. |
| `repository` | string | The repository name, without `.git`. |
| `access` | string | `read` or `write`. |

<!-- schema: serve.grant -->

```json
{
  "user": "bob",
  "repository": "demo",
  "access": "read"
}
```

`grit serve users`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `database` | string | The user database read. |
| `users` | array | Every user, sorted by name. |
| `users[].name` | string | User name. |
| `users[].admin` | boolean | Admins read and write every repository. |
| `users[].tokens` | integer | How many access tokens the user has. |
| `users[].grants` | array | Repository grants, sorted by repository: `repository` and `access` (`read` or `write`). |

<!-- schema: serve.users -->

```json
{
  "database": "/srv/git/grit-serve.db",
  "users": [
    {
      "name": "admin",
      "admin": true,
      "tokens": 1,
      "grants": []
    },
    {
      "name": "bob",
      "admin": false,
      "tokens": 2,
      "grants": [
        {
          "repository": "app",
          "access": "write"
        },
        {
          "repository": "docs",
          "access": "read"
        }
      ]
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/serve/v1.json) · [serve.add-user v1](/schemas/serve.add-user/v1.json) · [serve.grant v1](/schemas/serve.grant/v1.json) · [serve.users v1](/schemas/serve.users/v1.json)

Markdown uses headings, bullet lists, and tables; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

```text
## Serve users

Database: `/srv/git/grit-serve.db`

| User | Tokens | Access |
| --- | ---: | --- |
| `admin` | 1 | all repositories (admin) |
| `bob` | 2 | app (write), docs (read) |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)): no database, already set up, unknown user, or the server could not start. |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

## See also

[`push`](../push/) · [`fetch`](../fetch/) · [`clone`](../clone/) · [`upload-pack`](../upload-pack/) · [`receive-pack`](../receive-pack/)
