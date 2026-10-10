# grit fetch

> Download new commits, branches and tags from a remote.

## Synopsis

```text
grit fetch [<remote>]
```

## Description

Downloads everything new from a remote and updates your remote-tracking branches, such as `origin/main`, along with any new tags. Your own branches and working tree are left alone; to bring the new commits into your branch, use [`grit merge`](https://grit-scm.com/docs/merge/index.md) or [`grit pull`](https://grit-scm.com/docs/pull/index.md).

Each updated ref is listed with its old and new commit.

Repositories created as shallow clones with Git (for example `git clone --depth 1`) are supported: `grit fetch` respects the existing shallow boundary, does not create tag refs to missing commits, and leaves a clean repository when there is nothing new to download.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | The remote to fetch from. Defaults to `origin`. |

## Examples

```console
$ grit fetch
  refs/remotes/origin/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.
```

Fetch from another remote:

```console
$ grit fetch upstream
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `remote` | string | Remote that was fetched. |
| `updates` | array | Ref updates, each with `ref`, `old_oid`, and `new_oid`. |
| `updated` | number | Count of refs that changed. |

`old_oid` is `null` for a ref that is new, and `new_oid` is `null` for one that was removed.

Example:

```json
{
  "remote": "origin",
  "updates": [],
  "updated": 0
}
```

## See also

[grit pull](https://grit-scm.com/docs/pull/index.md), [grit merge](https://grit-scm.com/docs/merge/index.md), [grit remote](https://grit-scm.com/docs/remote/index.md)
