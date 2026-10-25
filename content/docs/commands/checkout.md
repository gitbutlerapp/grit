---
title: grit checkout
summary: Start work on a repository from a snapshot of its main branch, or of some of its paths, instead of a clone.
group: Getting started
order: 3
---

## Synopsis

```text
grit checkout <url> [<slice>] [-p|--path <path>]... [--into <dir>]
```

## Description

`grit checkout` makes a working repository from a server's newest commit without downloading its history. It asks the server for a **snapshot**: the target branch's tree (the branch the server's `HEAD` names), cut down to the paths you give, in a single commit. It downloads that snapshot and builds a normal Git repository around it: `git` and `grit` work in it as usual.

- With no `--path`, the snapshot is the whole tree.
- With `--path <dir>` (repeatable), it is everything under each path, plus the files directly in every directory above one, the top level included — the same files Git's cone-mode sparse checkout would have.
- With a `<slice>` name, it is that [slice](../slice/)'s paths (plus any `--path`), and the directory defaults to the slice's name.

The new repository's history is one commit, `Snapshot of <commit>: <subject>`, which stands for the real commit on the server. It is at `origin/<branch>` (which `origin/HEAD` names), and you are on a new **work branch** on top of it, named for your `user.email` and two random words (like `alice/amber-brook`). The server picks the final name: it numbers it (`alice/amber-brook-2`) when it already has that branch or has given the name to another checkout, and never gives a name out twice, even across restarts. There is no local copy of the target branch.

`origin` is the repository URL, set up so that plain `git` and `grit` commands stay with the checkout's snapshots:

```text
[remote "origin"]
	url = http://127.0.0.1:8080/demo.git
	fetch = +refs/checkouts/<id>/target:refs/remotes/origin/main
	push = +refs/heads/*:refs/checkouts/<id>/out/heads/*
	tagOpt = --no-tags
```

A plain `git fetch` or `grit fetch` brings only the checkout's own snapshots into `origin/main`, never the real history, other branches or tags. A plain `git push` or `grit push` sends your branches to the check-in area that [`grit checkin`](../checkin/) grafts from, never onto the server's real branches. `grit checkin` and `grit pull` themselves always talk to the server the checkout was made from, even if you point `origin` somewhere else.

What makes the repository a checkout is a small record in its git directory (`grit-checkout.json`): the server, the checkout's id, its target branch, its paths and its snapshots. Every worktree of the checkout (`git worktree add`) shares it, so checking in and refreshing work from any of them.

Snapshots are made the same way every time, so everyone who checks out the same paths at the same commit gets the same snapshot commit, and the server makes its download once.

The download resumes: if a checkout is interrupted, run the same command again and it picks up where it stopped (partial downloads are kept in `$XDG_CACHE_HOME/grit/checkouts`, `~/.cache/grit/checkouts` by default). The download has the same display as [`grit clone`](../clone/)'s bundle download.

Snapshot checkouts need a server that offers them, such as [`grit serve`](../serve/). They are served at their own endpoint, `<server>/.grit/checkouts/<repository>`, separate from Git's. When a server doesn't have it (a plain Git server, or a repository you can't read), `grit checkout` stops with an error and makes nothing:

```text
error: https://example.com/demo.git offers no grit checkout (no https://example.com/.grit/checkouts/demo endpoint); it may be a plain Git server, or the repository may not exist
```

Sign in to the server first with [`grit auth <url>`](../auth/).

Work comes back with [`grit checkin`](../checkin/), which rebuilds your branches on the server's real history. [`grit integrate <branch>`](../integrate/) merges one into the target branch, and [`grit pull`](../pull/) refreshes the checkout: the server makes a snapshot of the newest commit of the branch the checkout was made from (even if the server's `HEAD` has since moved to another branch) on top of your last one, `grit pull` fetches just the changes in your paths and rebases the current branch onto it. Commits already in the target drop out. A rebase that conflicts stops for you to resolve, then `grit rebase --continue`; other branches still on an older snapshot are named (see [`grit pull`](../pull/#in-a-snapshot-checkout)).

To switch branches, use [`grit switch`](../switch/) (also `grit co`).

## Options

| Option | Description |
| --- | --- |
| `<url>` | The repository, such as `http://127.0.0.1:8080/demo.git`. |
| `<slice>` | A [slice](../slice/) on the server whose paths to check out. |
| `-p`, `--path <path>` | Check out only this path (a directory or file), plus the files in the directories above it. Repeatable. |
| `--into <dir>` | Directory to check out into. Defaults to the repository's name. It must not exist or must be empty. |

## Examples

Check out one directory of a repository served by `grit serve`:

```console
$ grit checkout http://127.0.0.1:8080/demo.git --path src/deep
✓ Checked out /home/alice/demo on alice/birch-pebble in 117ms

  paths     src/deep
  files     4 files, 293 KiB as a 294 KiB download
  history   one commit, fd590c7, standing for main at 5a80f18

Commit on alice/birch-pebble as usual; origin/main is the snapshot.
$ cd demo && git log --oneline --all
fd590c7 Snapshot of 5a80f18e14a3: Add src and docs
```

## JSON output

Schema: [v1](/schemas/checkout/v1.json)

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The repository URL, configured as `origin` (with the fetch and push refspecs above). |
| `path` | string | Directory the checkout was made in. |
| `branch` | string | The server's target branch the snapshot was taken from. |
| `work` | string | The new branch checked out for your work. |
| `base` | string | The real commit of the target branch the snapshot stands for. |
| `snapshot` | string | The snapshot commit, the checkout's only commit. |
| `paths` | array of strings | The paths checked out; empty for the whole tree. |
| `files` | integer | Files in the snapshot. |
| `bytes` | integer | Bytes of those files. |
| `download_bytes` | integer | Size of the download. |
| `resumed_from` | integer | Bytes an earlier, interrupted checkout had already downloaded. |
| `id` | string | The server's id for the checkout. |
| `seconds` | number | Wall-clock seconds. |

```json
{
  "url": "http://127.0.0.1:8080/demo.git",
  "path": "demo",
  "branch": "main",
  "work": "alice/amber-elm",
  "base": "5a80f18e14a38d1fddfb042e0b7724901ae7b30f",
  "snapshot": "ebf573ce69beba70f4dfe136c515dc6e2a1899f7",
  "paths": [],
  "files": 5,
  "bytes": 300010,
  "download_bytes": 300744,
  "resumed_from": 0,
  "id": "a093149cf50483d816cc",
  "seconds": 0.13
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/checkout/v1.json)

```markdown
## Checkout complete

- **path**: `demo`
- **work branch**: `alice/birch-pebble`
- **url**: `http://127.0.0.1:8080/demo.git`
- **target**: `main` at `5a80f18e14a38d1fddfb042e0b7724901ae7b30f`
- **snapshot**: `fd590c7ae90eb06479c29d9640d83e72504483c1`
- **paths**: `src/deep`
- **files**: 4 (300010 bytes)
- **download**: 300744 bytes (resumed at 0)
```

## Exit status

| Code | Meaning |
| --- | --- |
| `0` | The checkout was made. |
| `1` | The server offers no snapshot checkouts for the repository, has no such slice, refused the request (for example a path it doesn't have), the destination is not empty, or the download failed. |

## See also

- [`grit serve`](../serve/) — serves snapshot checkouts.
- [`grit checkin`](../checkin/), [`grit integrate`](../integrate/), [`grit slice`](../slice/) and [`grit pull`](../pull/) — working in a checkout.
- [`grit clone`](../clone/) — a full copy with history.
