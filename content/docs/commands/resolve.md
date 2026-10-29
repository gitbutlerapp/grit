---
title: grit resolve
summary: Resolve revision specs to object ids and types.
group: Plumbing
order: 4
---

## Synopsis

```text
grit resolve [--short] [--abbrev-ref | --symbolic-full-name] [--verify <SPEC> [-q]] <SPEC>...
```

## Description

Resolves one or more revision specs to an object id, plus the object type and uncompressed size. Type and size follow `git cat-file` semantics, including `refs/replace` when replace refs are enabled (honor `GIT_NO_REPLACE_OBJECTS` to read the stored object instead). Supported forms match the library rev-parse surface: branches and tags, `HEAD~n`, peel suffixes such as `^{tree}`, path suffixes such as `main:path/file`, reflog selectors like `main@{1}` or `HEAD@{yesterday}` (date text uses the same relaxed grammar as `grit log --since`, for example `yesterday`, `last week`, `noon yesterday`, `Oct 27`, `10/27/2026`), abbreviated object ids, and **change ids** (reverse-hex tokens in `k`–`z`, 4–32 characters — stored header or derived id). See [Change ids — Revisions](../library/change-ids.md#revisions). When a date selector is older than every reflog entry, grit resolves to the oldest entry and emits a warning (same behavior as Git).

Each spec prints on its own line as `OID TYPE` (full hex by default). With `--short`, oids are abbreviated using the same uniqueness rules as elsewhere in grit.

The `:/<text>` message search resolves to the **youngest** matching commit by committer date among all commits reachable from any ref (including `HEAD`), not merely the first match walking back from one branch tip.

With `--abbrev-ref` or `--symbolic-full-name`, output switches to ref names (one line per spec): the shortest unambiguous short name, or the full symbolic ref name, matching `git rev-parse` for the same flags. Specs that are not refs (commit oids, peeled objects, unknown names) produce no line for that spec; exit status stays `0` unless a spec fails for another reason (for example `@{u}` with no upstream).

With `--verify <SPEC>`, exactly one spec is resolved strictly (like `git rev-parse --verify`): revision ranges (`A..B`, `A...B`), revision-set excludes (`^ref`), and other multi-revision forms are rejected with a typed error. Full hex ids must exist in the object database, and peel suffixes such as `^{commit}` are honored. Clap rejects extra positional specs when `--verify` is used (usage exit `2`). Success prints one object id line (abbreviated when `--short` is set). Combine with `-q` / `--quiet` to suppress all stdout and stderr and answer with exit status only (`0` = exists, `1` = not). `--json --quiet` prints nothing; the exit code alone answers.

## Options

| Option | Description |
| --- | --- |
| `<SPEC>…` | One or more revision specs to resolve (required). |
| `--short` | Print abbreviated object ids instead of full hex. |
| `--abbrev-ref` | Print the shortest unambiguous ref name for each spec (conflicts with default oid output). |
| `--symbolic-full-name` | Print the full symbolic ref name for each spec. |
| `--verify <SPEC>` | Resolve one spec strictly (`git rev-parse --verify`); conflicts with ref-name modes and extra positionals. |
| `-q`, `--quiet` | With `--verify`, suppress stdout and stderr; exit `0` or `1` only. |

## Examples

Resolve `HEAD` and its tree:

```console
$ grit resolve HEAD HEAD^{tree}
92501f188ae0815af09a0cef6e121eddda4113cf commit
a1b2c3d4e5f6789012345678901234567890abcd tree
```

Short ids:

```console
$ grit resolve --short main
92501f1 commit
```

## JSON output

Schema: [v1](/schemas/resolve/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `warnings` | array | Non-fatal warnings collected while resolving. |
| `results` | array | One entry per input spec. |
| `results[].spec` | string | Input spec. |
| `results[].oid` | string | Resolved object id (abbreviated when `--short` is set). |
| `results[].kind` | string | `blob`, `tree`, `commit`, or `tag`. |
| `results[].size` | number | Uncompressed object size in bytes. |
| `results[].ref_name` | string | In oid mode: short ref name when the spec named a ref (for example `main` for `HEAD`). In ref-name modes: the printed ref line (abbrev or full, matching the active flag). |
| `results[].full_ref` | string | Full symbolic ref when the spec named a ref (for example `refs/heads/main`). |
| `results[].ref_display` | string | Same as `ref_name` in ref-name modes (stable alias for scripts). |
| `results[].oid` / `kind` / `size` | | Required in default oid mode; omitted in ref-name modes. |

Example:

```json
{
  "results": [
    {
      "spec": "HEAD",
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "kind": "commit",
      "size": 234,
      "ref_name": "main",
      "full_ref": "refs/heads/main"
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/resolve/v1.json)

Pass `--markdown` for a table of resolved specs, oids, kinds, and sizes.



```text
## Resolve

| Spec | OID | Kind | Size | Ref |
| --- | --- | --- | ---: | --- |
| `HEAD` | `83352a61f6715ec3f0a297c6d6150725154fb833` | commit | 244 | HEAD |
| `HEAD^{tree}` | `cc5dd251b2645c614c83ffc5402450dfe72cedfd` | tree | 77 | HEAD |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Every spec resolved (default mode), or `--verify` / `--verify --quiet` resolved the spec. |
| `1` | At least one spec failed, or `--verify` / `--verify --quiet` could not resolve the spec. |
| `2` | Invalid flags ([`usage`](../exit-codes/)), or `--verify` with zero or multiple specs. |

See [Exit codes](../exit-codes/).


## See also

- [`grit cat`](cat.md) — show object contents for a spec
- [`grit show`](show.md) — commit-oriented summary with diffstat
