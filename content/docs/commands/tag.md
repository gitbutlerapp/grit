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
grit tag <name> -m <message>
grit tag <name> -s -m <message>
grit tag <name> --object <rev>
grit tag -d <name>
```

## Description

With no arguments, lists your tags sorted by **version** (`version:refname`). Use `-l` / `--list` with optional glob patterns (repeatable), `--sort version|date|name`, `--reverse`, `--contains [REV]` (defaults to `HEAD`), `--points-at <REV>` (repeatable), and `-n` / `--limit` to cap the list. Annotated tags show the subject on the human line; `--json` includes full `annotation` metadata.

With a name and no `-m`, creates a **lightweight** tag pointing at `HEAD` (or `--object`). With `-m`, creates an **annotated** tag object with that message. Use `-s` / `--sign` (or `tag.gpgSign=true`) to cryptographically sign an annotated tag; signed tags verify with `git verify-tag` and system `git tag -v`.

Tag names must be valid as `refs/tags/<name>` under Git's ref-name rules (same checks as `git check-ref-format refs/tags/<name>`). Invalid names are rejected before writing under `.git/refs`.

Tags are local until you publish them with [`grit push --tags`](../push/). Delete a remote tag with [`grit push --delete`](../push/).

## Options

| Option | Description |
| --- | --- |
| `<name>` | The tag to create or delete. Omit to list tags. |
| `-l`, `--list` | List tags matching `PATTERN` (repeatable). Omit `PATTERN` to list all. |
| `--sort` | `version` (default), `date`, or `name`. |
| `--reverse` | Reverse the sort order. |
| `--contains` | Only tags whose peeled commit contains `REV` (default `HEAD`). |
| `--points-at` | Only tags pointing at `REV` (repeatable). |
| `-n`, `--limit` | Maximum number of tags to print. |
| `-d`, `--delete` | Delete the tag locally. |
| `-m`, `--message` | Annotated tag message. |
| `-s`, `--sign` | Sign the tag object (implies annotated; honors `tag.gpgSign` when omitted). |
| `--no-sign` | Do not sign, even when `tag.gpgSign` is true. |
| `--signing-key` | Signing key id or path for this tag (overrides `user.signingkey`). |
| `--object` | Revision to tag (default `HEAD`). |

## Examples

```console
$ grit tag v0.1 -m 'First release'
Created tag v0.1

$ grit tag v0.2 -s -m 'Signed release'
Created tag v0.2

$ grit tag
  v0.1

$ grit push --tags
  pushed --tags → origin refs/tags/v0.1

$ grit push --delete v0.1
  pushed --delete → origin refs/tags/v0.1

$ grit tag -d v0.1
Deleted tag v0.1
```

## JSON output

Schema: [v1](/schemas/tag/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `tags` | array | For `list`, each tag with `name`, `oid`, `kind`, `target_kind`, optional `peeled`, and optional `annotation` (`subject`, `message`, `tagger`, `date`). |
| `name` | string | For `create` or `delete`, the tag name. |
| `oid` | string | For `create`, full id the tag ref points at (tag object or commit). |
| `kind` | string | For `create` or list entries: `lightweight` or `annotated`. |
| `signed` | boolean | For `create`, whether the tag object was signed. |
| `peeled` | string | Commit oid when the tag peels to a commit. |

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
  "peeled": "abc1234567890abcdef1234567890abcdef123456"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/tag/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit push](../push/)
