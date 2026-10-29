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
grit push [<remote>] --delete <ref>...
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

Pushes refs to a remote over SSH, smart HTTP, or the `file://` transport. With no refspecs, grit pushes the current branch using the same remote-selection rules as Git (`branch.<name>.pushRemote`, `remote.pushDefault`, `branch.<name>.remote`, then `origin`) and applies `remote.<name>.push` refspecs when configured.

Incremental SSH and HTTP pushes stop exploring advertised commit history once the new commit frontier is exhausted. A one-commit push does not need to walk every advertised branch and tag back to its root. Annotated tags targeting blobs or trees may be advertised alongside commit refs without preventing a push.

On the remote, `receive.denyNonFastForwards`, `receive.denyDeletes`, and `receive.denyCurrentBranch` are enforced for every transport, including local-path remotes (not only [`grit receive-pack`](../receive-pack/) over HTTP).

A push to a local path or `file://` URL runs the remote repository's `pre-receive`, `update`, `post-receive` and `post-update` hooks just as [`grit receive-pack`](../receive-pack/) and Git do: the pushed objects wait in a quarantine that `pre-receive` can read, a non-zero `pre-receive` refuses every update (`pre-receive hook declined`) and keeps the objects out of the repository, a non-zero `update` refuses its own ref (`hook declined`), and `post-receive` and `post-update` run after the refs are written. Hook output is reported like any remote's messages. `--dry-run` runs none of these hooks, as with Git.

Explicit refspecs follow Git push rules: any revision on the left (`HEAD`, tags, branches), optional `+` for force, `:dst` to delete, and a bare branch name DWIMs to the matching local ref (then through configured push refspecs when present).

`--dry-run` negotiates with the remote and reports what would change without updating refs or sending objects.

With `--tags`, pushes every local tag under `refs/tags/`.

With `--delete`, every positional after the remote names a remote branch or tag to delete (short names map like Git), and nothing is pushed. All of them are deleted in one push, with one result per ref. They must be plain ref names: a `src:dst` refspec next to `--delete` is an error and nothing is sent, as is `--delete` with no ref. To publish one ref and delete another, run two pushes or use a `:dst` refspec without `--delete` (`grit push origin HEAD:release :old`). `--delete` cannot be combined with `--tags`.

`--force-with-lease` updates a remote ref only when it still matches the local remote-tracking ref mapped from the push destination (or when the ref is absent if there is no tracking ref). Use `--force-with-lease=<rev>` when pushing exactly one ref to compare against a specific commit. `--force` overwrites the remote ref unconditionally. The two flags cannot be combined.

`-u` / `--set-upstream` writes `branch.<name>.remote` and `branch.<name>.merge` for each successfully pushed branch (skipped with `--dry-run`).

`--atomic` requires every ref update in the push to succeed or none are applied, including when a remote update hook rejects one ref. Over SSH, `git://`, and smart HTTP, the server must advertise the `atomic` capability. If it does not, grit fails before sending ref updates or objects; it never falls back to a non-atomic push. This capability check also applies to `--dry-run`.

`-o` / `--push-option` sends server-side push options (for example `ci.skip`), which the remote's `pre-receive` and `post-receive` hooks see as `GIT_PUSH_OPTION_COUNT` and `GIT_PUSH_OPTION_<n>`. The remote must accept them: a server advertises the `push-options` capability, and a local-path or `file://` remote must set `receive.advertisePushOptions`. Otherwise the push fails before anything is sent, `--dry-run` included.

`--follow-tags` pushes annotated tags that point to commits reachable from the refs being pushed (lightweight tags and unreachable annotated tags are skipped). The config key `push.followTags` enables the same behavior by default.

In human output, messages the remote sends while it processes the push (hook output, CI links, policy text) are shown on stderr as `remote: …` lines, including when stderr is not a terminal. `-q` / `--quiet` hides them and the human summary, so a successful push prints nothing; a rejected push is still reported and exits `1`. With `--json` or `--markdown`, the same lines appear in `remote_messages` (omitted when empty).

Before anything is sent, grit runs the repository's `pre-push` hook (see [`grit hooks`](../hooks/)) once the remote's current refs are known, on every transport and with `--dry-run` too. The hook gets the remote name (or the URL, for an unnamed remote) and the push URL as arguments, and one line per ref update on stdin, `<local ref> <local oid> <remote ref> <remote oid>`, in Git's format: `(delete)` and a zero object id for deletions, a zero remote object id for new refs, and no line for refs that are up to date or already rejected. If the hook exits non-zero, the push stops, the remote is left untouched and grit exits `1`. The hook's stdout is sent to stderr so `--json` and `--markdown` output stays clean. `--no-verify` skips the hook.

### Large pushes

When a smart-HTTP server advertises a resumable push endpoint (the `pushBundle` capability, as [`grit serve`](../serve/) does) and the pack to send is larger than 16 MiB, grit writes the pack as a bundle and uploads it with the [TUS](https://tus.io/protocols/resumable-upload) resumable-upload protocol instead of one long request, then asks the server to apply the ref updates. The result is reported like any other push. A dropped connection is retried from the server's offset, and an interrupted push keeps its bundle and upload in `.git/grit/push-bundles/`: run the same `grit push` again and it skips building the pack and sends only the rest.

```text
↻ Resuming push bundle upload at 12.0 MiB of 29.6 MiB (41%)  example.com/demo/push/97d00bc69d6359be08d3a13753f95d2a
  ✓ push bundle 29.6 MiB in 3.4s · 5.20 MiB/s · resumed at 12.0 MiB
```

Set `push.bundleThreshold` (bytes, with optional `k`, `m` or `g`) to change the size at which this starts.

For GitHub over HTTPS, if a push fails because you aren't signed in, `grit` offers to run [`grit auth`](../auth/) and then tries again.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | Remote name (optional; defaults from branch / config). |
| `<refspec>...` | Push refspecs (`feature`, `HEAD:refs/heads/x`, `+main:main`, `:gone`, …). |
| `-t`, `--tags` | Push all local tags instead of the current branch. |
| `--delete` | Delete the remote refs named by the positionals instead of pushing (plain names only). |
| `--dry-run` | Contact the remote but do not update refs or send objects. |
| `-f`, `--force` | Overwrite remote refs when the update is not a fast-forward. |
| `--force-with-lease` | Push only if the remote ref matches the local remote-tracking ref (or is absent when there is no tracking ref). |
| `--force-with-lease=<rev>` | Same check against `<rev>` (exactly one ref must be pushed). |
| `-u`, `--set-upstream` | Set upstream config for pushed branches. |
| `--atomic` | All ref updates succeed or none are applied; fails before sending updates if the server does not support atomic pushes. |
| `-o`, `--push-option` | Server-side push option (repeatable; the remote must accept push options). |
| `--follow-tags` | Push annotated tags reachable from the commits being pushed. |
| `-q`, `--quiet` | Suppress remote messages and the human summary of a successful push. |
| `--no-verify` | Skip the `pre-push` hook. |
| `--remote` | Hidden alias for the remote name (prefer the positional `<remote>`). |

## Examples

```console
$ grit push
  pushed main → origin refs/heads/main

$ grit push origin feature
  pushed feature → origin refs/heads/feature

$ grit push origin v1
  pushed v1 → origin refs/tags/v1

$ grit push origin HEAD:refs/heads/bench
  pushed refs → origin refs/heads/bench

$ grit push --dry-run
  would push main → origin refs/heads/main

$ grit push --tags
  pushed refs → origin refs/tags/v0.1

$ grit push --delete v0.1
  deleted v0.1 from origin

$ grit push origin --delete old-topic stale-fix
  deleted old-topic from origin
  deleted stale-fix from origin
```

A refspec next to `--delete` is refused before anything is sent:

```console
$ grit push origin HEAD:refs/heads/release --delete old-topic
error: cannot delete 'HEAD:refs/heads/release': --delete takes ref names, not src:dst refspecs
$ echo $?
1
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

When another push created the same new branch while yours was in flight (each ref update is compare-and-swapped against what the push saw, so the other push's commit is never overwritten, even with `--force`):

```console
$ grit push origin HEAD:refs/heads/shared
  rejected origin refs/heads/shared: created on the remote meanwhile — run `grit fetch` and try again
```

When the `pre-push` hook refuses the push (nothing is sent):

```console
$ grit push
pre-push: secrets found in config/prod.env
error: hook pre-push failed with exit status 1
$ echo $?
1
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

For other HTTPS hosts the hint points at `grit auth <url> --username <user> --token <token>`, which saves a token with your credential helper, or `GIT_ASKPASS`; when a helper is configured but has nothing for the host, the hint names it. A stored credential the server refuses fails with `auth_rejected` (and the helper drops it, as in Git) rather than claiming none exist. When no helper has the credentials, an askpass program (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`) is asked for them, as in Git (see [Credentials](../agents/#credentials-https)). With `--json` these failures are `{"error", "kind", "hint"}` objects with `kind` `no_remote`, `remote_not_found`, `remote_no_url`, `auth_required` or `auth_rejected` (see [Exit codes](../exit-codes/)).

## JSON output

Schema: [v1](/schemas/push/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was pushed to. |
| `kind` | string | `branch`, `tags`, `delete`, or `refspecs`. |
| `branch` | string or null | Short branch name when `kind` is `branch`, or the deleted branch’s short name when `kind` is `delete` and exactly one branch is deleted. Omitted for tag and other explicit refspec pushes (`kind`: `refspecs`). |
| `dry_run` | boolean | `true` when `--dry-run` was used (no refs or objects written). |
| `results` | array | Per-ref outcomes; each object has `ref`, `status` (`ok`, `up_to_date`, or `rejected`), optional `reason` (`stale` for a failed lease or a ref another push moved meanwhile, `already_exists` when another push created the new ref first, `remote_ref_missing` when `--delete` targets a ref absent on the remote), optional `deleted` (`true` when the ref was removed), `old_oid`, `new_oid`, `src`, and `forced`. |
| `rejected` | boolean | `true` if any ref was rejected (exit status is also `1`). |
| `upstream_set` | array of strings | Branch short names whose upstream config was written (`-u` only; omitted when empty). |
| `remote_messages` | array of strings | Hook and diagnostic text from the remote (same lines as `remote: …` on stderr in human mode; omitted when empty). |

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

When the `pre-push` hook fails, the push exits **1** with `{"error":"hook pre-push failed with exit status 1"}` on stdout and nothing is sent.

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
| `1` | Command failed, a ref was rejected, or the `pre-push` hook exited non-zero ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |
| `128` | Could not reach or read the remote: missing repository, DNS or connection failure, HTTP error status ([`remote_unreadable`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit pull](../pull/), [grit tag](../tag/), [grit auth](../auth/)