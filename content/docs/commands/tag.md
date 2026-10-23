---
title: grit tag
summary: List tags, or create or delete one (lightweight or annotated).
group: Branches and tags
order: 5
---

## Synopsis

```text
grit tag
grit tag -l [<pattern>...]
grit tag --sort version|date|name [--reverse] [--contains [REV]] [--points-at REV] [-n N]
grit tag <name>
grit tag <name> [<target>]
grit tag <name> -m <message>
grit tag <name> -F <path|->
grit tag <name> -s -m <message>
grit tag <name> -s --tree-sha256 -m <message>
grit tag <name> -f
grit tag <name> --dry-run
grit tag -d <name>
grit tag --verify <name>...
grit tag -v <name>... --no-signature
```

## Description

With no arguments, lists your tags sorted by **version** (`version:refname`). Use `-l` / `--list` with optional glob patterns (repeatable), `--sort version|date|name`, `--reverse`, `--contains [REV]` (defaults to `HEAD`), `--points-at <REV>` (repeatable), and `-n` / `--limit` to cap the list. Annotated tags show the subject on the human line; `--json` includes full `annotation` metadata.

With a name and no `-m`/`-F`, creates a **lightweight** tag pointing at `HEAD` or the optional `<target>` revision. With `-m` or `-F`, creates an **annotated** tag object with that message (comment lines starting with `#` are stripped, matching Git's default cleanup). Use `-s` / `--sign` (or `tag.gpgSign=true`) to cryptographically sign an annotated tag; signed tags verify with `git verify-tag` and system `git tag -v`.

Use `-f` / `--force` to replace an existing tag (lightweight or annotated). The previous ref target is reported in `--json` as `replaced`.

Use `--dry-run` on create or delete to preview the outcome without changing refs.

Use `--tree-sha256` on a signed tag to attach a [tree-sha256](../library/tree-sha256.md) content digest of the tagged commit's tree (implies signing). Repeat `--exclude-submodule <glob>` to record excluded submodule paths in `tree-sha256-excluded` headers.

Use `--verify` / `-v` on one or more **annotated** tag names to check the tag object's signature (OpenPGP or SSH, when present) and, when the tag carries a `tree-sha256` header, recompute that digest against the tagged commit's tree. Lightweight tags cannot be verified. Use `--no-signature` to skip signature checking while still validating `tree-sha256` when present.

**Exit codes:** `grit tag --verify` exits **0** when every named tag passes and **1** when any tag fails verification or is not an annotated tag. Other `grit tag` modes use the default exit code (**0** on success).

Tag names must be valid as `refs/tags/<name>` under Git's ref-name rules (same checks as `git check-ref-format refs/tags/<name>`). Invalid names are rejected before writing under `.git/refs`.

Tags are local until you publish them with [`grit push --tags`](../push/). Delete a remote tag with [`grit push --delete`](../push/).

## Options

| Option | Description |
| --- | --- |
| `<name>` | The tag to create or delete. Omit to list tags. |
| `<target>` | Revision to tag (default `HEAD`). |
| `-l`, `--list` | List tags matching `PATTERN` (repeatable). Omit `PATTERN` to list all. |
| `--sort` | `version` (default), `date`, or `name`. |
| `--reverse` | Reverse the sort order. |
| `--contains` | Only tags whose peeled commit contains `REV` (default `HEAD`). |
| `--points-at` | Only tags pointing at `REV` (repeatable). |
| `-n`, `--limit` | Maximum number of tags to print. |
| `-d`, `--delete` | Delete the tag locally. |
| `-m`, `--message` | Annotated tag message. |
| `-F`, `--message-file` | Read the annotated tag message from a file (`-` reads stdin). |
| `--file` | Hidden alias for `--message-file` (deprecated; emits a one-time warning). |
| `-f`, `--force` | Replace an existing tag with the same name. |
| `--dry-run` | Preview create or delete without updating refs. |
| `-s`, `--sign` | Sign the tag object (implies annotated; honors `tag.gpgSign` when omitted). |
| `--no-sign` | Do not sign, even when `tag.gpgSign` is true. |
| `--signing-key` | Signing key id or path for this tag (overrides `user.signingkey`). |
| `--tree-sha256` | Attach a tree-sha256 digest (implies `--sign`). |
| `--exclude-submodule` | Submodule path glob excluded from the digest (repeatable). |
| `-v`, `--verify` | Verify annotated tags (one or more names). |
| `--no-signature` | With `--verify`, skip signature checks (digest still checked when present). |
| `--object` | Hidden alias for `<target>` (one release). |

## Examples

```console
$ grit tag v0.1 -m 'First release'
Created tag v0.1

$ grit tag v0.2 -s -m 'Signed release'
Created tag v0.2

$ grit tag v0.1 main -f -m 'Repointed release'
Created tag v0.1

$ grit tag
  v0.1

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1

$ grit push --delete v0.1
  pushed --delete → origin refs/tags/v0.1

$ grit tag -d v0.1
Deleted tag v0.1

$ grit tag --verify v0.2 --no-signature
tag v0.2 (92501f1…): ok
  target: abc1234…
  signature: not_checked (—)
  elapsed: 12 ms
```

## JSON output

Schema: [v1](/schemas/tag/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, `delete`, or `verify`. |
| `tags` | array | For `list`, each tag with `name`, `oid`, `kind`, `target_kind`, optional `peeled`, and optional `annotation` (`subject`, `message`, `tagger`, `date`). |
| `truncated` | boolean | For `list`: `true` when more tags matched than returned (`-n` / `--limit`). |
| `name` | string | For `create` or `delete`, the tag name. |
| `oid` | string | For `create`, full id the tag ref points at (tag object or commit). |
| `kind` | string | For `create` or list entries: `lightweight` or `annotated`. |
| `signed` | boolean | For `create`, whether the tag object was signed. |
| `peeled` | string | Commit oid when the tag peels to a commit. |
| `replaced` | string | For `create` with `--force`, oid the tag ref pointed at before replacement. |
| `dry_run` | boolean | For `create` or `delete`, whether refs were left unchanged. |
| `tree_sha256` | string | For `create` with `--tree-sha256`, hex digest recorded on the tag. |
| `tree_sha256_excluded` | array | Submodule paths excluded from the digest (when `--exclude-submodule` is used). |
| `tree_sha256_files` | integer | Blob count hashed for the tree-sha256 claim. |
| `tree_sha256_submodules` | integer | Submodule trees included in the digest walk. |
| `tree_sha256_hash_ms` | number | Wall time spent computing the tree-sha256 digest (milliseconds). |
| `ok` | boolean | For `verify`, whether every tag passed. |

For `action: verify`, each element of `tags` includes `name`, `oid`, `target`, `ok`, `signature` (with `verdict`, optional `signer`, `key`, `trust`), optional `tree_sha256` (`claimed`, `computed`, `match_digest`, `files`, `submodules`, `excluded`, `bogus_excluded`, `cycles`), `not_verifiable_submodules` (`path`, `reason`), and `elapsed.ms`.

Listing tags:

```json
{
  "action": "list",
  "tags": [
    {
      "name": "v0.1",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "kind": "annotated",
      "target_kind": "commit",
      "peeled": "abc1234567890abcdef1234567890abcdef123456",
      "annotation": {
        "subject": "First release",
        "message": "First release\n",
        "tagger": { "name": "Alex", "email": "alex@example.com" },
        "date": 1700000000
      }
    }
  ]
}
```

Creating an annotated tag:

```json
{
  "action": "create",
  "name": "v0.1",
  "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "kind": "annotated",
  "signed": false,
  "dry_run": false,
  "peeled": "abc1234567890abcdef1234567890abcdef123456"
}
```

Verifying tags:

```json
{
  "action": "verify",
  "ok": true,
  "tags": [
    {
      "name": "v0.2",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "target": "abc1234567890abcdef1234567890abcdef123456",
      "ok": true,
      "signature": {
        "verdict": "good",
        "signer": "Release Bot <bot@example.com>",
        "key": "ABCD1234",
        "trust": "fully"
      },
      "tree_sha256": {
        "claimed": "a119881755f081b9f694f1f05eb31fa4771a108bc854d61fdd4d12482e2a14e2",
        "computed": "a119881755f081b9f694f1f05eb31fa4771a108bc854d61fdd4d12482e2a14e2",
        "match_digest": true,
        "files": 3,
        "submodules": 0,
        "excluded": [],
        "bogus_excluded": [],
        "cycles": []
      },
      "not_verifiable_submodules": [],
      "elapsed": { "ms": 12 }
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/tag/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.



```text
No tags yet.
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success, or `--verify` when every tag passes. |
| `1` | `--verify` when any tag fails or is not annotated. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit push](../push/)