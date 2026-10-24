---
title: grit diff
summary: Show uncommitted, staged, commit, or range changes.
group: Making changes
order: 3
---

## Synopsis

```text
grit diff [--staged] [--unstaged] [--against <ref>] [<commit> [<commit>]]
  [--pathspec-from-file <file|->] [--pathspec-file-nul] [-- <path>…]
grit diff [--staged] [--unstaged] [--against <ref>] [<base>..<tip>]
  [--pathspec-from-file <file|->] [--pathspec-file-nul] [-- <path>…]
grit diff --no-index <path> <path>
```

## Description

With no argument, shows every uncommitted change: the working tree and index together, compared with the last commit (`HEAD`). Pass `--staged` to compare only the index to `HEAD` (staged changes). Pass `--unstaged` to compare only the working tree to the index (changes not yet staged). Pass `--against <ref>` to compare the working tree to another ref’s tree instead of `HEAD` (useful for “what changed since `main`?” while keeping a single commit argument as “show this commit’s patch”).

With one commit, shows the change that commit introduced (its first parent’s tree against its own tree). Commit, range, **staged**, and **unstaged** diffs detect renames by default (Git `-M50`): a move with small edits appears as one `renamed` file instead of a full delete and add. With two commits, or a two-dot range such as `A..B`, shows the tree diff between those commits with the same rename detection. A three-dot range `A...B` diffs from the merge base of `A` and `B` to `B`.

Pass one or more paths after `--` (or as trailing arguments when unambiguous) to limit any diff form to those paths—the same pathspec rules as [`grit log`](log.md). Pathspec magic works as in Git, including attribute selection from `.gitattributes`: `grit diff -- ':(attr:diff=rust)'` limits the diff to files with `diff=rust`, and `':(exclude,attr:-diff)'` drops files marked `-diff`. A single positional argument that is neither a revision nor a range is still treated as a worktree path for unstaged changes (`grit diff a.txt`). With **two** positional arguments, both are revisions (tree diff); a path limit requires `--` (for example `grit diff HEAD~1 HEAD -- f`) or the `commit known-path` form when the second token names a file in the repo (`grit diff HEAD f`).

Each file starts with its path (UTF-8 names print literally; C-quoted only when the name contains control characters, unless `core.quotepath=true`), and each hunk with its position. Lines have two number columns, the line number before and after the change. In a terminal, removed lines are red, added lines are green, and the words that changed within a line are highlighted when the diff touches at most 64 changed files; larger tree and commit-range diffs stay line-level for throughput. When the output isn't a terminal, or `NO_COLOR` is set, lines are marked with `-` and `+` instead. `--json` and `--markdown` use line-level hunks only (no word-level `emphasis` segments) so large commit-range diffs stay fast. Large tree diffs honor the global `--threads` flag and `index.threads` when loading blobs and computing line diffs. Three lines of unchanged context are shown around each change. Binary files print `Binary file differs` when the blob changed (mode-only binary changes show mode lines only).

`<commit>` can be anything that names a commit: a full or short commit id, a branch or tag name, or an expression like `HEAD~2`.

## Options

| Option | Description |
| --- | --- |
| `-p`, `--patch` | Full patch (the default). Mutually exclusive with the other format flags below. |
| `--stat` | Per-file change counts with `+`/`-` bars and a summary line (like `git diff --stat`). Mutually exclusive with the other format flags below. |
| `--numstat` | Machine-readable `insertions<TAB>deletions<TAB>path` lines (`-<TAB>-` for binary). |
| `--shortstat` | Only the summary line (`N files changed, …`). |
| `--name-only` | Changed paths, one per line. |
| `--name-status` | Status letter and path(s) per line (`M`, `A`, `R100<TAB>old<TAB>new`, …). |
| `--patch-text` | Git-apply-compatible unified patch on stdout (`git diff --binary` shape). Human output only; not combinable with `--json` or `--markdown`. |
| `-M`, `--find-renames[=<n>]` | Detect renames (default similarity 50). Long form requires `=` when a threshold is given. |
| `--no-renames` | Do not detect renames. |
| `--diff-algorithm <algo>` | Line diff algorithm (`myers`, `minimal`, `patience`, `histogram`). |
| `--diff-filter <letters>` | Limit to change types (`A` added, `D` deleted, `M` modified, `R` renamed, …). |
| `-w`, `--ignore-all-space` | Ignore whitespace when comparing lines. |
| `-b`, `--ignore-space-change` | Ignore changes in the amount of whitespace. |
| `--ignore-blank-lines` | Ignore changes whose lines are all blank. |
| `-U`, `--unified <n>` | Lines of context around each hunk when the output is a patch (bare `-U` means 3). |
| `-M`, `--find-renames[=<n>]` | Detect renames; optional similarity threshold (`50` default, or `50%`). Attached `-M30` is accepted. |
| `--no-renames` | Do not detect renames (show separate delete and add entries). |
| `--diff-algorithm <algo>` | Line diff algorithm (`myers`, `patience`, `histogram`, …); overrides `diff.algorithm`. |
| `--diff-filter <letters>` | Limit output to change types (`A` added, `D` deleted, `M` modified, `R` renamed, …). |
| `-w`, `--ignore-all-space` | Ignore whitespace when comparing lines. |
| `-b`, `--ignore-space-change` | Ignore changes in the amount of whitespace. |
| `--ignore-blank-lines` | Ignore changes whose lines are all blank. |
| `-W`, `--function-context` | Expand patch hunks to enclosing function boundaries (Git `-W`). |
| `-p`, `--patch` | Unified patch output (default when no other format flag is set). |
| `--exit-code` | Exit with status **1** when there are changes (for scripts); **0** when clean. |
| `--quiet` | Print nothing; implies `--exit-code`. |
| `--staged`, `-S` | Staged changes only (index vs `HEAD`). Cannot be combined with `--unstaged`, commit arguments, or `--against`. Optional path limits after `--` are allowed. |
| `--unstaged` | Unstaged changes only (worktree vs index). Cannot be combined with `--staged`, commit arguments, or `--against`. Optional path limits after `--` are allowed. |
| `--against <ref>` | Compare the worktree to `<ref>`’s tree instead of `HEAD`. No commit arguments. Optional path limits after `--` are allowed. |
| `--no-index` | Compare two filesystem paths without a repository: two files, two directories (recursive), or a file and a directory (compared with `directory/<file name>`, as Git does). Takes exactly two paths and cannot be combined with `--staged`, `--unstaged`, `--against` or commits. Exits 1 when they differ, as with `--exit-code`. |
| `--exit-code` | Exit with status 1 when there are changes and 0 when there are none; errors exit 2. |
| `--quiet` | Print nothing and exit as with `--exit-code`. |
| `<commit>` | One commit (that commit’s patch), two commits (tree diff), or a range `A..B` / `A...B`. Omit for uncommitted changes. |
| `--pathspec-from-file <file\|->` | Read pathspecs from a file (`-` for stdin), in addition to argv paths after `--`. |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated. |
| `<path>…` | After `--`, limit the diff to these paths (all forms above, including `--staged`). |

## Examples

What have I changed since the last commit?

```console
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

What is staged but not yet committed?

```console
$ grit diff --staged
```

What changed in the working tree since the last `grit add`?

```console
$ grit diff --unstaged
```

What changed between two commits?

```console
$ grit diff HEAD~1 HEAD
$ grit diff HEAD~1..HEAD
```

Only one file between two branches:

```console
$ grit diff main feature -- README.md
```

What does my worktree look like compared to `main`?

```console
$ grit diff --against main
```

What did the commit before the last one change?

```console
$ grit diff HEAD~1
```

List the files changed by a commit:

```console
$ grit diff v0.1 --json --filter '.files[].path'
[
  "README.md",
  "main.rs"
]
```

Compare two files or directories that aren't in a repository:

```console
$ grit diff --no-index ./old ./new
```

## JSON output

Schema: [v1](/schemas/diff/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `changed` | boolean | Whether any selected change exists (matches `--exit-code` / `--quiet`). |
| `files` | array | Changed files (see nested fields below). |
| `totals` | object | Present for `--stat`, `--numstat`, and `--shortstat`: `files_changed`, `insertions`, `deletions`. |
| `changed` | boolean | Whether any change was selected; the same answer `--exit-code` gives as an exit status. |

Each `files[]` entry includes `path`, `status`, `binary`, optional `old_path` (renames), optional `similarity` (0–100 on renames/copies), optional `combined_status` (per-parent combined letters such as `MM` or `DD` on merge `--cc` / `--name-status` output; omitted on ordinary two-tree diffs), optional `stat` (`{ "insertions", "deletions" }` when line counts were computed; omitted when `binary` is true), optional `old_mode` / `new_mode`, optional `old_encoding_lossy` / `new_encoding_lossy`, and `hunks` only for the default patch format.

Nested fields include `files[].status` (`added`, `modified`, `deleted`, `renamed`, and so on), optional `files[].old_path` when `status` is `renamed`, `hunks[].old_start` / `new_start` / `old_lines` / `new_lines`, optional `hunks[].id` (twelve lowercase hex characters; present on `--staged` and `--unstaged` patch JSON for stable hunk selection), optional `hunks[].index` (0-based hunk index within the file; same scopes as `id`), optional `hunks[].context` (enclosing definition line), `hunks[].lines[].kind` (`context`, `add`, or `del`), line numbers on `old` and `new`, a compact `text` string per line (or `segments` with optional `emphasis` only when intra-line word highlights are present), and `no_newline_at_eof` when that side lacks a trailing newline (Git’s `\ No newline at end of file`). JSON is written in compact form (no pretty-printing). When `files[].binary` is true, `hunks` is always an empty array — blob bytes are never included in JSON (use human or `--markdown` output for the “Binary file differs” note). For unstaged and other worktree-sourced sides, line text is normalized to repository form (`.gitattributes` / `core.autocrlf`) before diffing, so trailing `\r` from CRLF checkouts does not appear in line `text`. Commit and staged diffs compare stored blobs and may still show carriage returns when the on-disk object bytes differ. When file bytes are not valid UTF-8 and the file is treated as text, `old_encoding_lossy` or `new_encoding_lossy` is true and non-UTF-8 bytes appear as U+FFFD in text fields.

Example:

```json
{
  "changed": true,
  "files": [
    {
      "path": "staged.txt",
      "status": "added",
      "new_mode": "100644",
      "binary": false,
      "hunks": [
        {
          "old_start": 0,
          "new_start": 1,
          "old_lines": 0,
          "new_lines": 1,
          "lines": [
            {
              "kind": "add",
              "new": 1,
              "text": "staged contents"
            }
          ]
        }
      ]
    },
    {
      "path": "tracked.txt",
      "status": "modified",
      "old_mode": "100644",
      "new_mode": "100644",
      "binary": false,
      "hunks": [
        {
          "old_start": 1,
          "new_start": 1,
          "old_lines": 1,
          "new_lines": 1,
          "lines": [
            {
              "kind": "del",
              "old": 1,
              "text": "tracked v2"
            },
            {
              "kind": "add",
              "new": 1,
              "text": "tracked modified"
            }
          ]
        }
      ]
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/diff/v1.json)
Pass [`--markdown`](../global-options/) for one section per changed file: a level-2 heading, then metadata lines (`**status:**`, and `**old_mode:**` / `**new_mode:**` when the git mode changed), then a `diff`-fenced unified patch when there are text hunks (binary files note `Binary file differs.` when content changed). Mode-only or rename-only changes omit the fence when the patch would be empty.

```text
## main.rs
**status:** modified
```

When there are text hunks, a unified patch follows in a `diff` code fence:

```diff
@@ -1,1 +1,2 @@
 fn main() {
+    println!("hello");
 }
```

Mode-only changes list modes and omit an empty fence:

```text
## run.sh
**status:** modified
**old_mode:** `100644`
**new_mode:** `100755`
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | No changes selected, or success without `--exit-code` / `--quiet`. |
| `1` | Changes exist when `--exit-code` or `--quiet` is set ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or diff error while `--exit-code` / `--quiet` is set ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for the full table.


## See also

[grit status](../status/), [grit show](../show/), [grit log](../log/)