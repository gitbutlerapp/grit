---
title: grit push
summary: Publish branches, tags, or explicit refspecs to a remote.
group: Remotes
order: 4
---

## Synopsis

```text
grit push [<remote>] [<refspec>...]
grit push --tags
grit push --delete <ref>
grit push --dry-run
grit push --remote <name>   # hidden alias for one release
```

## Description

Pushes refs to a remote over the smart HTTP or `file://` transport. With no refspecs, grit pushes the current branch using the same remote-selection rules as Git (`branch.<name>.pushRemote`, `remote.pushDefault`, `branch.<name>.remote`, then `origin`) and applies `remote.<name>.push` refspecs when configured.

Explicit refspecs follow Git push rules: any revision on the left (`HEAD`, tags, branches), optional `+` for force, `:dst` to delete, and a bare branch name DWIMs to the matching local ref (then through configured push refspecs when present).

`--dry-run` negotiates with the remote and reports what would change without updating refs or sending objects.

With `--tags`, pushes every local tag under `refs/tags/`.

With `--delete`, deletes remote branches or tags (short names map like Git).

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](../auth/) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote name (optional; defaults from branch / config). |
| `<refspec>...` | Push refspecs (`feature`, `HEAD:refs/heads/x`, `+main:main`, `:gone`, …). |
| `-t`, `--tags` | Push all local tags instead of the current branch. |
| `--delete` | Delete a remote ref (repeatable). |
| `--dry-run` | Contact the remote but do not update refs or send objects. |
| `--remote` | Hidden alias for the remote name (prefer the positional `<remote>`). |

## Examples

```console
$ grit push
  pushed main → origin refs/heads/main

$ grit push origin feature
  pushed feature → origin refs/heads/feature

$ grit push origin HEAD:refs/heads/bench
  pushed refs → origin refs/heads/bench

$ grit push --dry-run
  would push main → origin refs/heads/main

$ grit push --tags
  pushed refs → origin refs/tags/v0.1

$ grit push --delete v0.1
  pushed --delete → origin refs/tags/v0.1
```

When someone else pushed first:

```console
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## JSON output

Schema: [v1](/schemas/push/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `kind` | string | `branch`, `tags`, `delete`, or `refspecs`. |
| `branch` | string? | Short branch name when `kind` is `branch` (omitted otherwise). |
| `dry_run` | boolean | Present and `true` when `--dry-run` was used. |
| `results` | array | Per-ref outcomes (see below). |
| `rejected` | boolean | `true` if any ref was rejected (exit status is also `1`). |

Each element of `results`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `ref` | string | Remote ref updated. |
| `status` | string | `ok`, `up_to_date`, or `rejected`. |
| `reason` | string? | Rejection detail when `status` is `rejected`. |
| `old_oid` | string? | Previous remote tip (full hex). |
| `new_oid` | string? | New tip after a successful update. |
| `forced` | boolean | `true` for a non-fast-forward update with force. |
| `src` | string? | Local source ref when known. |

Example:

```json
{
  "remote": "origin",
  "kind": "branch",
  "branch": "main",
  "results": [
    {
      "ref": "refs/heads/main",
      "status": "ok",
      "old_oid": null,
      "new_oid": "83352a61f6715ec3f0a297c6d6150725154fb833",
      "forced": false,
      "src": "refs/heads/main"
    }
  ],
  "rejected": false
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/push/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit pull](../pull/), [grit tag](../tag/), [grit auth](../auth/)
