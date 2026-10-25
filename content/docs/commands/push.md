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
grit push --force-with-lease[=<rev>]
grit push --force
grit push -u [--set-upstream]
grit push --atomic
grit push -o <option> [--push-option <option>]
grit push --follow-tags
grit push -q [--quiet]
grit push --remote <name>   # hidden alias for one release
```

## Description

Pushes refs to a remote over the smart HTTP or `file://` transport. With no refspecs, grit pushes the current branch using the same remote-selection rules as Git (`branch.<name>.pushRemote`, `remote.pushDefault`, `branch.<name>.remote`, then `origin`) and applies `remote.<name>.push` refspecs when configured.

On the remote, `receive.denyNonFastForwards`, `receive.denyDeletes`, and `receive.denyCurrentBranch` are enforced for every transport, including local-path remotes (not only [`grit receive-pack`](../receive-pack/) over HTTP).

Explicit refspecs follow Git push rules: any revision on the left (`HEAD`, tags, branches), optional `+` for force, `:dst` to delete, and a bare branch name DWIMs to the matching local ref (then through configured push refspecs when present).

`--dry-run` negotiates with the remote and reports what would change without updating refs or sending objects.

With `--tags`, pushes every local tag under `refs/tags/`.

With `--delete`, deletes remote branches or tags (short names map like Git).

`--force-with-lease` updates a remote ref only when it still matches the local remote-tracking ref mapped from the push destination (or when the ref is absent if there is no tracking ref). Use `--force-with-lease=<rev>` when pushing exactly one ref to compare against a specific commit. `--force` overwrites the remote ref unconditionally. The two flags cannot be combined.

`-u` / `--set-upstream` writes `branch.<name>.remote` and `branch.<name>.merge` for each successfully pushed branch (skipped with `--dry-run`).

`--atomic` requires every ref update in the push to succeed or none are applied (when the transport supports it).

`-o` / `--push-option` sends server-side push options (for example `ci.skip`) on smart HTTP; `file://` remotes reject non-empty push options.

`--follow-tags` pushes annotated tags that point to commits reachable from the refs being pushed (lightweight tags and unreachable annotated tags are skipped). The config key `push.followTags` enables the same behavior by default.

On an interactive terminal (human output), messages the remote sends while it processes the push, such as hook output, are shown on stderr as `remote: …` lines. `-q` / `--quiet` hides them and the human summary, so a successful push prints nothing; a rejected push is still reported and exits `1`. `--json` and `--markdown` output is unchanged.

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](../auth/) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote name (optional; defaults from branch / config). |
| `<refspec>...` | Push refspecs (`feature`, `HEAD:refs/heads/x`, `+main:main`, `:gone`, …). |
| `-t`, `--tags` | Push all local tags instead of the current branch. |
| `--delete` | Delete a remote ref (repeatable). |
| `--dry-run` | Contact the remote but do not update refs or send objects. |
| `-f`, `--force` | Overwrite remote refs when the update is not a fast-forward. |
| `--force-with-lease` | Push only if the remote ref matches the local remote-tracking ref (or is absent when there is no tracking ref). |
| `--force-with-lease=<rev>` | Same check against `<rev>` (exactly one ref must be pushed). |
| `-u`, `--set-upstream` | Set upstream config for pushed branches. |
| `--atomic` | All ref updates succeed or none are applied. |
| `-o`, `--push-option` | Server-side push option (repeatable; smart HTTP only). |
| `--follow-tags` | Push annotated tags reachable from the commits being pushed. |
| `-q`, `--quiet` | Suppress remote messages and the human summary of a successful push. |
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

When the remote moved since your last fetch:

```console
$ grit push --force-with-lease
  rejected origin refs/heads/main: stale remote ref — run `grit fetch` and try again
```

When the delete target is not on the remote:

```console
$ grit push origin --delete no-such-branch
  rejected origin refs/heads/no-such-branch: remote ref does not exist
```

When there is nowhere to push yet, or the server wants credentials, the error says so and the `hint:` line names the next command:

```console
$ grit push
error: no remote to push to
hint: add one with `grit remote add origin <url>`, or push to a URL with `grit push <url> <branch>`

$ grit push upstream main
error: unknown remote 'upstream'
hint: add it with `grit remote add upstream <url>`, or pass a URL or path instead; `grit remote` lists the configured remotes

$ grit push
error: authentication failed for https://github.com/ada/project.git: credentials required but unavailable (non-interactive)
hint: sign in to GitHub with `grit auth` and try again; if it still fails, check the URL
```

For other HTTPS hosts the hint points at a `credential.helper` instead of `grit auth`. With `--json` these failures are `{"error", "kind", "hint"}` objects with `kind` `no_remote`, `remote_not_found`, `remote_no_url`, `auth_required` or `auth_rejected` (see [Exit codes](../exit-codes/)).

## JSON output

Schema: [v1](/schemas/push/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `kind` | string | `branch`, `tags`, `delete`, or `refspecs`. |
| `branch` | string or null | Short branch name when `kind` is `branch`. |
| `dry_run` | boolean | `true` when `--dry-run` was used (no refs or objects written). |
| `results` | array | Per-ref outcomes; each object has `ref`, `status` (`ok`, `up_to_date`, or `rejected`), optional `reason` (`stale` for a failed lease, `remote_ref_missing` when `--delete` targets a ref absent on the remote), `old_oid`, `new_oid`, `src`, and `forced`. |
| `rejected` | boolean | `true` if any ref was rejected (exit status is also `1`). |
| `upstream_set` | array of strings | Branch short names whose upstream config was written (`-u` only; omitted when empty). |

Example:

```json
{
  "remote": "origin",
  "kind": "branch",
  "branch": "main",
  "dry_run": false,
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




```text
## Push to `origin`
- **`refs/heads/main`** · pushed `main` → `origin refs/heads/main`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |
| `128` | Could not reach or read the remote: missing repository, DNS or connection failure, HTTP error status ([`remote_unreadable`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit pull](../pull/), [grit tag](../tag/), [grit auth](../auth/)