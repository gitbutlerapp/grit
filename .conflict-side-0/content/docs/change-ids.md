---
title: Change ids
summary: Stable change identifiers on commits, Git interop, and revision selection.
---

Every commit [`grit commit`](../commit/) creates can carry a **`change-id`** header: an opaque token that names the same logical change across rewrites (amend, [`grit history`](../history/) edits, and future rebases). Commits that never received the header still have a **derived** id (Jujutsu-compatible) computed from the commit object id. Grit never rewrites a commit solely to add a header.

## Header format

- On disk: a normal commit header line `change-id <value>` (before any `gpgsig` block).
- Legacy name: `gitbutler-change-id <value>` is still read for compatibility; new writes use `change-id` only.
- Random ids use **reverse-hex**: 16 bytes encoded as 32 characters in the alphabet `k`–`z` (each nibble `n` maps to `'z' - n`).

System **`git`** treats the line like any other commit header: it round-trips through clone and fetch, survives `git cat-file -p`, and passes `git fsck --strict`. Git’s `%(trailers)` pretty-format does not surface `change-id` (it is not a Git trailer), so use `cat-file` or grit’s JSON when you need the stored value.

## Interop with Jujutsu, GitButler, and Gerrit

| Tool | How it relates |
| --- | --- |
| **Jujutsu (`jj`)** | Derives a change id from the commit id with the same algorithm grit uses when no header is present. After grit stores a derived id on rewrite, jj and grit agree on the logical change. |
| **GitButler** | Historically wrote `gitbutler-change-id`; grit reads that name and writes `change-id` on new commits. Stacks and virtual branches can key off the same string grit shows in `log` and `--json`. |
| **Gerrit** | Uses `Change-Id` in the commit message for review matching. Grit’s header is separate (object header, not message footer). Repos imported from Gerrit keep their message trailers; grit adds its own header only on commits it creates. |

## Configuration

| Key | Default | Effect |
| --- | --- | --- |
| [`grit.changeIds`](../config/) | `true` | When `false`, new commits omit the header (byte-identical with plain Git). Existing headers are preserved; history rewrites still copy them but do not invent derived ids. |
| `grit.testing.changeId` | unset | Test-only: `content-hash` makes ids deterministic; any other non-empty value is used verbatim. |

Disable headers when an importer rejects extra commit lines:

```console
$ grit config grit.changeIds false
```

## Display and JSON

**Human [`grit log`](../log/)** (default format) shows the short commit id, then `change` and the first eight reverse-hex characters of the stable id, then author, date, and subject. [`--oneline`](../log/) omits the change column.

**[`--json`](../commands/README.md#commit-json-object-commitjson)** on [`log`](../log/), [`show`](../show/), [`revs`](../revs/), and [`status`](../status/) includes:

| Field | Meaning |
| --- | --- |
| `change_id` | Stable id (stored header or derived from `oid`). |
| `change_id_stored` | `true` when `change_id` came from a `change-id` / `gitbutler-change-id` header in the object. |

Mutation commands ([`commit`](../commit/), [`pick`](../pick/), [`revert`](../revert/), [`merge`](../merge/), [`pull`](../pull/), [`history`](../history/)) report `change_id` on the commits they create. [`grit commit --json`](../commit/) sets `change_id` to the stored header value or JSON `null` when the header is absent (for example with `grit.changeIds=false`).

## Which commits get ids

- **New ids:** ordinary commits, the initial commit, merge conclusions, pick, revert, and commits inserted during a history rewrite.
- **Same id kept:** amend, history reword/fixup/squash/drop/replay when the logical change survives.
- **Derived id stored on rewrite:** headerless commits (for example from plain `git commit`) receive a stored `change-id` equal to the derived value the first time grit rewrites them.

Pick and revert always allocate a **new** id; they never copy the source commit’s id.

## Revisions

After refs and abbreviated object ids, grit accepts **change ids** as revision tokens:

- **Alphabet:** reverse-hex tokens use `k`–`z`, length 4–32. Such tokens are resolved as change-id prefixes, not Git object ids.
- **Lookup:** grit walks commits reachable from refs and `HEAD`, matches [`resolved_change_id`](rustdoc:grit_lib::change_id::resolved_change_id) (stored or derived), and requires a unique prefix.
- **Composition:** suffixes work as usual — `\<cid\>~1`, `\<cid\>^`, ranges, peel operators, and `:path` forms compose on the resolved commit.
- **Precedence:** a branch or tag with the same name wins over a change-id token.

Examples:

```console
$ grit show krrtwksq --json
$ grit log yrwyvtvk -n 1 --json
$ grit history reword krrtwksq -m "new subject"
```

Ambiguous prefixes fail with a list of candidate commits (same as ambiguous short oids).

## Library API

For embedders, see the [library guide on change ids](../library/change-ids/) (`ChangeId`, `ChangeIdPolicy`, header helpers, and lookup).
