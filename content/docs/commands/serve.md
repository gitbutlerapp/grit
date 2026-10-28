---
title: grit serve
summary: Serve repositories over HTTP, with a web view where people sign in with email and password.
group: Remotes
order: 7
---

## Synopsis

```text
grit serve [--root <path>] [--db <file>] [--bind <host:port>] [--bundle-interval <duration>] [--no-bundles]
grit serve --setup [--email <email>] [--root <path>] [--db <file>]
grit serve add-user <email> [--admin] [--root <path>] [--db <file>]
grit serve token <email> [--root <path>] [--db <file>]
grit serve grant <email> <repository> [--write] [--root <path>] [--db <file>]
grit serve users [--root <path>] [--db <file>]
```

## Description

`grit serve` runs a Git server over HTTP. Any Git client, `git` or `grit`, can clone, fetch and push through it, and a browser shows each repository's branches and recent commits. Fetch speaks Git protocol v0, v1 and v2 (whichever the client asks for); push uses receive-pack, which has no v2 form. Everything runs in-process: the server needs no `git` or other `grit` binary.

`--root` names what to serve:

- a **directory of repositories**: each child directory that is a repository (bare `project.git` or a working tree `project`) is served as `project`. New repositories show up without a restart.
- a **single repository**: it is the only one served, under its directory name.

Clients reach a repository at `http://<host>:<port>/<name>.git` (the `.git` is optional).

### Users and passwords

A user is an email address with a password. Users, their access and their tokens live in a SQLite database, `grit-serve.db` in the root by default (or in the root's `.git` directory when the root is a working tree, so the file never shows up as untracked). Pick another path with `--db`.

- `grit serve --setup` creates the database and the first **admin** user. It asks for the admin's email (or takes `--email`) and a password. An admin can read and write every repository, and add users in the web view.
- `grit serve add-user <email>` adds a user. Add `--admin` for another admin.
- `grit serve grant <email> <repository>` gives a user read access (fetch and clone) to one repository; `--write` also allows pushing. Granting again replaces the earlier access.
- `grit serve users` lists every user, their token count and their access.

Both `--setup` and `add-user` ask for the password twice without echoing it. Leave it empty to have one generated instead: a **temporary password** like `5qy3-ewqt-mgjj-3jnv`, printed once, which the user must replace with their own the first time they sign in. Without a terminal, the password is read from the first line of stdin, and empty input generates one. Passwords need at least 8 characters; the database keeps only an Argon2 hash.

### Signing in

People sign in to the web view at `http://<host>:<port>/` with their email and password. There they can change their password, and admins get a **Users** page to add users and give them access. Adding a user there generates a temporary password to send them, together with their email address, yourself.

Git uses access tokens instead of passwords. To get one, run [`grit auth <url>`](../auth/) (or `grit login <url>`) with the server's address or any repository URL on it. It opens the server's sign-in page in your browser and waits; after you sign in and approve the code it shows, it saves a new token with your credential helper, so `grit` and `git` clone, fetch and push without asking.

For scripts and CI, which cannot sign in in a browser, an admin can make a token with `grit serve token <email>` and hand it over: `grit auth <url> --token <token>` stores it, a URL carries it (`git clone http://bob:<token>@host:8080/project.git`, where the user name is ignored), or a client can send `Authorization: Bearer <token>`. Each token is printed once; the database keeps only a SHA-256 hash of it. A token starts with `grit_`.

A Git request without a valid token gets `401`, and a browser without a session is sent to the sign-in page. A repository the user cannot read answers `404`, the same as one that does not exist, and a push from a user with read access answers `403`.

### Clone bundles

The server keeps a prebuilt [bundle](https://git-scm.com/docs/git-bundle) of each repository's branches and tags in `grit-serve/bundles/` inside its Git directory. A background thread builds a repository's first bundle as soon as the server starts (or as soon as the repository gets its first branch), and builds a new one after pushes have changed its refs, once the current bundle is older than `--bundle-interval` (24 hours by default; `0s` rebuilds right after every push). Each bundle gets a new file name and the previous one is kept, so a client part way through downloading the old bundle can still finish it.

Fetches over protocol v2 see the `bundle-uri` command in the server's capabilities. It answers with the current bundle's URL, `http://<host>:<port>/<name>/bundles/<file>`, which serves the file with `Range` support so an interrupted download can resume. [`grit clone`](../clone/) downloads the bundle first and then fetches only what is newer; `git clone` does the same with `-c transfer.bundleURI=true` and a credential helper that knows the token. Downloading a bundle needs read access, like fetching.

`--no-bundles` turns this off: nothing is built and nothing is advertised.

A repository with promisor remotes gets no bundle, and any bundle built earlier is removed. This covers a repository offloaded with [`grit gc offload`](../gc/#grit-gc-offload): some of its blobs live elsewhere, and a bundle holding them would undo the offload. Its clients clone through upload-pack, which points them at the promisor.

### Partial clone filters

[`grit clone --blobs`](../clone/) and filtered fetches need the server to advertise Git's `filter` capability. On each served bare repository, enable it the same way as with `git upload-pack`:

```console
$ git -C /path/to/root/project.git config uploadpack.allowFilter true
```

Filter support is off by default (matching stock `git upload-pack`). Without it, clients that pass `--blobs` or `--filter` still connect, but the server sends a full pack; grit warns when that happens.

### Resumable pushes

The push advertisement carries a `pushBundle=/push` capability: a [TUS](https://tus.io/protocols/resumable-upload) upload endpoint at `http://<host>:<port>/<name>/push`. [`grit push`](../push/) sends a large push there as a bundle, in chunks that survive a dropped connection, then posts the usual ref updates to `…/push/<id>/receive`. The server applies them exactly as for a normal push (the same checks, policy and hooks) and answers with the usual push report. Uploading needs write access, and an upload belongs to the user who started it. Unfinished uploads are deleted after a week. Git clients ignore the capability and push the usual way.

| Request | Answer |
| --- | --- |
| `OPTIONS /<name>/push` | TUS version and extensions (`creation`) |
| `POST /<name>/push` with `Upload-Length` | `201` and the upload's `Location` |
| `HEAD /<name>/push/<id>` | `Upload-Offset`: bytes received so far |
| `PATCH /<name>/push/<id>` with `Upload-Offset` | appends the body; answers the new offset |
| `POST /<name>/push/<id>/receive` | applies the bundle with the posted ref updates |

### Snapshot checkouts

[`grit checkout <url>`](../checkout/) starts work on a repository without its history: the server makes a snapshot commit of the target branch (where `HEAD` points), cut down to the paths asked for, and serves it as a resumable download. These requests have their own endpoint, apart from Git's, and need read access:

| Request | Answer |
| --- | --- |
| `GET /.grit/checkouts/<name>` | the repository's name, target branch and its tip |
| `POST /.grit/checkouts/<name>` | makes a checkout of the paths in the JSON body |
| `GET /.grit/checkouts/<name>/<id>` | the checkout |
| `GET /.grit/checkouts/<name>/<id>/bundle` | its snapshot as a bundle, with `Range` support |
| `POST /.grit/checkouts/<name>/<id>/update` | a snapshot of the newest tip of the branch the checkout was made from, on top of the checkout's last one, at `refs/checkouts/<id>/target` (`grit pull`) |
| `POST /.grit/checkouts/<name>/<id>/checkin` | grafts branches pushed to `refs/checkouts/<id>/out/heads/*` onto the real history and sets them (`grit checkin`; write access) |
| `POST /.grit/checkouts/<name>/<id>/branch` | shows the checkout the server's branch named in the JSON body (`name`): its commits the target lacks, cut down to the checkout's paths on its snapshots, at `refs/checkouts/<id>/in/heads/<branch>` (`grit pull <branch>`) |
| `POST /.grit/checkouts/<name>/<id>/work` | reserves a fresh work-branch name for the checkout, from the `work` name in the JSON body, as making a checkout does (`grit integrate`, moving you off the branch it merged) |
| `POST /.grit/checkouts/<name>/integrate` | merges a branch into the target with a merge commit (`grit integrate`; write access) |
| `GET /.grit/checkouts/<name>/slices` | the slices in `.grit/slices.toml` on the target |
| `PUT`, `DELETE /.grit/checkouts/<name>/slices/<slice>` | changes a slice with a commit on the target (`grit slice`; write access) |

The server keeps checkouts in `grit-serve/checkouts/` inside the Git directory (each checkout's record in `<id>.json`, replaced whole so a crash never leaves half of one, the work-branch names it has given out under `names/`, so a name is never given out twice, and the graft log under `grafts/`: which real commit each checkout commit stands for, filed by the first two hex digits of each side and only ever appended to) and points `refs/checkouts/<id>/target` at each one's snapshot. Requests on one checkout take turns, and so do check-ins setting the repository's branches: of several check-ins of one branch at once, the first sets it and the others are refused. Branches are set with a compare-and-swap, so a branch that moved meanwhile is decided again rather than overwritten. A branch the server has moves only as a fast-forward, when the same checkout set it last, when the new branch keeps every commit the old one had beyond the target by change id (a rebase of it whose copies keep what the originals changed outside the checking-in checkout's paths), or when the check-in is forced; the server's log line and the check-in's answer name the rule (see [`grit checkin`](../checkin/)). A grafted commit with no `change-id` header gets the one derived from the checkout commit's own id. Check-ins record the commits they graft in the graft log once the branch is set (a log that can't be written is reported in the server's log, and the check-in still succeeds), and so does showing a checkout a branch, so work built on a branch a checkout was shown grafts onto the real commits it stands for, and a check-in replays only commits it hasn't grafted before; a check-in's walk stops at the target branch, so a branch built on the real history is refused at once. A shown commit keeps its author, date and message, gets a change id derived from the real commit when it has none, and drops signatures; a checkout's own commits (those its pushed branches at `refs/checkouts/<id>/out/heads/*` reach) come back as they are, and another checkout's only when they have the tree the projection would make. Where the repository ignores case (`core.ignorecase`), a branch asked for in another case is shown under the server's spelling (see [`grit pull`](../pull/#a-server-branch-in-a-snapshot-checkout)).

Once a repository has checkouts, its target branch changes only through integrations and slice changes. Pushes that create, update or delete it are refused, by `git` and `grit` clients alike, over smart HTTP and the resumable endpoint (and by [`grit receive-pack`](../receive-pack/)); branches whose names differ from it only in case are refused too. The refusal (`main is the target branch; it changes only through integrate`, or Git's own `branch is currently checked out` when the repository has it checked out) comes with instructions, which Git clients show as `remote:` lines: push another branch and run [`grit integrate <branch>`](../integrate/). A repository without checkouts keeps Git's rules for pushes, including refusing to delete the branch `HEAD` names (`receive.denyDeleteCurrent`).

Integrations and slice changes take turns, one at a time per repository, and each starts over on the new tip if the target moves while it is made (by another process). Integrating refuses names that are not valid branch names, the target itself and branches that share no history with it. A server-made commit's author comes from the request (the client's own author identity) or is the signed-in user; `<`, `>` and line breaks are cleaned out of it, so it can't break the commit's header. When the repository has a working tree with the target checked out (`HEAD` names it), that tree and its index move with each integration and slice change, keeping uncommitted changes there as `git reset --keep` does; a change that would overwrite them is refused and the target stays where it was. Every ref move the server makes is logged where the repository keeps reflogs (`core.logAllRefUpdates`, on by default in a repository with a working tree), as `checkin: from checkout <id>`, `integrate: Merge branch '<branch>' into <target>` or `slice: <message>`, so `git reflog <branch>` shows it and `<branch>@{1}` undoes it.

### Large-object promisors

A repository can point clients at promisor remotes that hold some of its objects, such as a large-object promisor (LOP) with the big blobs. Set `promisor.advertise = true` and give the repository a promisor remote (`remote.<name>.url` and `remote.<name>.promisor = true`); the protocol v2 capabilities then carry a `promisor-remote` line naming it, and `promisor.sendFields` adds its `partialCloneFilter` and `token`. A LOP served by `grit serve` needs `uploadpack.allowFilter` and `uploadpack.allowAnySHA1InWant`, so clients can fetch blobs no ref points at. See [grit upload-pack](../upload-pack/#promisor-remotes) for the exchange, and [grit clone](../clone/) for the client side.

Over smart HTTP every request stands alone, so the server only knows a client accepted a promisor remote when the `fetch` request itself says so. `grit` clients send their acceptance with every command; `git` clients send it with `ls-refs` only, so for them the server behaves as if nothing was accepted.

To keep a repository's big blobs out of it, host the LOP as a second repository next to it and run [`grit gc offload`](../gc/#grit-gc-offload) on the main one. Point `remote.<lop>.url` at the URL clients should use (for example `http://<host>:<port>/<lop>.git` on this server), and pass `--lop-dir` with the LOP's directory on disk. The main repository then serves clients that accept the LOP without the offloaded blobs. A client that accepts nothing is refused with a hint, unless the server may fetch the blobs itself: set `uploadpack.lazyFetch = true` in the main repository, or start `grit serve` with `GIT_NO_LAZY_FETCH=0`. Only those upload-pack requests get a promisor fetcher; snapshot checkouts, clone bundles and pushes never do. So a server without lazy fetch never fetches anything, and nothing pulls an offloaded blob back into the main repository: a snapshot checkout that needs one fails instead. Pushes may name offloaded blobs in their trees. If the LOP sits behind this server's authentication, the server needs credentials for it, like any other client. See [Serving a repository whose large blobs live elsewhere](../upload-pack/#serving-a-repository-whose-large-blobs-live-elsewhere).

### Running it

`grit serve` with no subcommand and no `--setup` runs the server until interrupted. It refuses to start until `--setup` has created a user. It listens on `127.0.0.1:8080`; use `--bind 0.0.0.0:8080` to accept connections from other machines. The server speaks plain HTTP, so put it behind a TLS-terminating proxy before sending passwords and tokens across a network you don't trust.

## Options

| Option | Description |
| --- | --- |
| `--root <path>` | A repository, or a directory of repositories, to serve. Defaults to the current directory. Also accepted after a subcommand. |
| `--db <file>` | The user database. Defaults to `grit-serve.db` in the root (in its `.git` directory for a working tree). Also accepted after a subcommand. |
| `--bind <host:port>` | Address to listen on. Defaults to `127.0.0.1:8080`. |
| `--setup` | Create the database and its first admin user, then exit. Prompts for a password, or generates a temporary one. Fails when the database already has users. |
| `--email <email>` | Email address of the admin `--setup` creates. Asked for on a terminal when omitted. Requires `--setup`. |
| `--bundle-interval <duration>` | How old a changed repository's clone bundle gets before a new one is built: a number with `s`, `m`, `h` or `d`. Defaults to `24h`. |
| `--no-bundles` | Do not build or advertise clone bundles. |
| `add-user <email>` | Add a user, named by their email address. Prompts for a password, or generates a temporary one. |
| `add-user --admin` | Make the new user an admin. |
| `token <email>` | Create an access token for a user and print it, for scripts and CI. |
| `grant <email> <repository>` | Give a user read access to a repository, by name with or without `.git`. |
| `grant --write` | Give write (push) access instead of read access. |
| `users` | List users, their tokens and their access. |

## Examples

Set up a directory of repositories and run the server:

```console
$ cd /srv/git
$ grit serve --setup --email admin@example.com
Password for admin@example.com (leave empty to generate a temporary one):
Repeat it:
✓ Created admin user admin@example.com in ./grit-serve.db
$ grit serve
Serving 1 repository from /srv/git at http://127.0.0.1:8080/
  http://127.0.0.1:8080/demo.git
Sign in at http://127.0.0.1:8080/ (command line: grit auth http://127.0.0.1:8080)
Clone bundles are built in the background and rebuilt every 24h after pushes
bundle: built 1792924806-0bfb630026ae.bundle (30309803 bytes) for /srv/git/demo
```

Sign in from another terminal, approve the code in the browser, then clone and push:

```console
$ grit auth http://127.0.0.1:8080
To sign in, open this page in your browser:

    http://127.0.0.1:8080/.grit/authorize?code=K7MQ-3XZP

and check that it shows the code:

    K7MQ-3XZP

(Opened it for you.)
Waiting for you to approve… (press Ctrl-C to cancel)
✓ Signed in to 127.0.0.1:8080 as admin@example.com — token stored.
$ grit clone http://127.0.0.1:8080/demo.git
```

Add a user with a generated password who can only fetch `demo`:

```console
$ grit serve add-user bob@example.com
Password for bob@example.com (leave empty to generate a temporary one):
✓ Created user bob@example.com in ./grit-serve.db

  Email:    bob@example.com
  Password: 5qy3-ewqt-mgjj-3jnv

The password is temporary and not shown again: it must be changed at the first sign-in.
$ grit serve grant bob@example.com demo
✓ bob@example.com now has read access to demo
$ grit serve users
admin@example.com  1 token    all repositories (admin)
bob@example.com    0 tokens   demo (read)  (temporary password)
```

A push from `bob` is refused:

```console
$ git push origin main
remote: bob@example.com has read-only access to demo
fatal: unable to access 'http://127.0.0.1:8080/demo/': The requested URL returned error: 403
```

Give a CI job its own token:

```console
$ grit serve token bob@example.com
✓ Created a token for bob@example.com

  Token: grit_617db60c2d54601c6109c9a9e7aaeac0dd3c5307

Save it now; it is not shown again. Use it as the password when Git asks, or run `grit auth <url> --token <token>`.
```

Rebuild bundles an hour after pushes, or not at all:

```console
$ grit serve --bundle-interval 1h
$ grit serve --no-bundles
```

Serve a single repository from its working tree:

```console
$ grit serve --root ~/projects/app --setup --email me@example.com
$ grit serve --root ~/projects/app --bind 0.0.0.0:8080
```

## JSON output

Schema: [v1](/schemas/serve/v1.json) · [serve.add-user v1](/schemas/serve.add-user/v1.json) · [serve.token v1](/schemas/serve.token/v1.json) · [serve.grant v1](/schemas/serve.grant/v1.json) · [serve.users v1](/schemas/serve.users/v1.json)

Pass `--json` with `--setup` or a subcommand for one JSON object on stdout. The running server has no JSON output: it logs to stderr.

`grit serve --setup` and `grit serve add-user`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `database` | string | The user database the user was added to. |
| `user` | string | The new user's email address. |
| `admin` | boolean | Whether the user is an admin (read and write on every repository). |
| `password` | string or omitted | The generated temporary password, shown only once. Omitted when the password was typed in. |
| `temporary` | boolean | Whether the user must choose a new password at their first sign-in (`true` for a generated password). |

<!-- schema: serve.add-user -->

```json
{
  "database": "./grit-serve.db",
  "user": "carol@example.com",
  "admin": false,
  "password": "y3hm-yphb-9y4a-2sdd",
  "temporary": true
}
```

`grit serve token`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `user` | string | The user's email address. |
| `token` | string | The new access token. It is shown only once. |

<!-- schema: serve.token -->

```json
{
  "user": "bob@example.com",
  "token": "grit_2d8cd88f35edd8b65b6d8d3a1023dd2225f87866"
}
```

`grit serve grant`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `user` | string | The email address of the user given access. |
| `repository` | string | The repository name, without `.git`. |
| `access` | string | `read` or `write`. |

<!-- schema: serve.grant -->

```json
{
  "user": "bob@example.com",
  "repository": "demo",
  "access": "read"
}
```

`grit serve users`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `database` | string | The user database read. |
| `users` | array | Every user, sorted by email address. |
| `users[].email` | string | The user's email address. |
| `users[].admin` | boolean | Admins read and write every repository. |
| `users[].temporary_password` | boolean | The user still has a generated password and must choose their own at the next sign-in. |
| `users[].tokens` | integer | How many access tokens the user has. |
| `users[].grants` | array | Repository grants, sorted by repository: `repository` and `access` (`read` or `write`). |

<!-- schema: serve.users -->

```json
{
  "database": "/srv/git/grit-serve.db",
  "users": [
    {
      "email": "admin@example.com",
      "admin": true,
      "temporary_password": false,
      "tokens": 1,
      "grants": []
    },
    {
      "email": "bob@example.com",
      "admin": false,
      "temporary_password": true,
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

Schema: [v1](/schemas/serve/v1.json) · [serve.add-user v1](/schemas/serve.add-user/v1.json) · [serve.token v1](/schemas/serve.token/v1.json) · [serve.grant v1](/schemas/serve.grant/v1.json) · [serve.users v1](/schemas/serve.users/v1.json)

Markdown uses headings, bullet lists, and tables; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

```text
## Serve users

Database: `/srv/git/grit-serve.db`

| User | Tokens | Access |
| --- | ---: | --- |
| `admin@example.com` | 1 | all repositories (admin) |
| `bob@example.com` | 2 | app (write), docs (read) |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)): no database, already set up, unknown user, an invalid email address or too short a password, or the server could not start. |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

## See also

[`auth`](../auth/) · [`push`](../push/) · [`fetch`](../fetch/) · [`clone`](../clone/) · [`upload-pack`](../upload-pack/) · [`receive-pack`](../receive-pack/)
