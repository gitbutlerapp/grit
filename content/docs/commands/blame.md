---
title: grit blame
summary: Show who last modified each line of a file.
group: History
order: 2
---

## Synopsis

```text
grit blame [<rev>] [--rev <commit>] [-L RANGE] [--first-parent] [--group] [-w]
  [-C]... [--ignore-rev <REV>]... [--ignore-revs-file <FILE>]... [--] <file>
```

## Description

Walks history for one file and prints each line with the commit that last modified it, the author, when that commit was made, and the line number in the blamed revision. The file operand uses Git pathspec cwd rules (same as [`grit diff`](../diff/)): from a subdirectory, a bare name like `f.txt` refers to that path under your current directory.

Blame visits commits newest first (by committer date) and keeps one entry per commit that still owns lines. When a merge hands lines to several parents and those branches meet again at an older commit, their lines join that commit's entry, so a commit is normally diffed against its parents once, however many merge paths reach it (a commit whose timestamp is older than one of its descendants can be visited again when clocks were skewed). File versions that no queued commit needs are dropped once more than 64 are cached, and parent/child line maps are cached only when `--ignore-rev` or `-C` can ask for the same pair twice. On git.git's `Makefile` (4.1k lines touched across thousands of merges) that is about 1.2 s and 335 MB peak RSS on a 4-core Linux VM, against about 1.0 s and 290 MB for system `git blame`. For each parent step, lines still being tracked that fall in an unchanged prefix or suffix are mapped to the parent without running a full-file diff; when every remaining line is in those affix regions, the step skips diffing entirely. With `-L`, that often avoids almost all diffs on large files whose recent edits are far from the requested range (for example tail-only churn). Stretches of history where the file does not change are skipped by reading only each commit's header and the tree entries on the file's path (unchanged subdirectories are not re-read), and no tree data is kept once its entry is found, so memory stays flat however long the history is: blaming `README.md` in git.git (22.5k first-parent commits) takes about 0.37 s and 290 MB peak RSS, against about 0.29 s and 245 MB for system `git blame`. On this repository, blaming `grit-lib/src/pack.rs` (~4.6k lines, ~90 commits) completes in a few seconds on a release build; system `git blame` on the same path is still faster on some very large real-world files, and parity work continues.

When a commit renamed or moved the file (including rename-and-edit in the same commit), blame follows the predecessor path using rename detection with Git's default similarity threshold (50%, like `git blame` with `-M`). This behavior is independent of `diff.renames` (system Git blame still follows renames when that diff setting is off). Git's `diff.renameLimit` and `status.renameLimit` cap how many rename pairings diff/status consider; they do not change blame's similarity percentage.

On Unix, explicit file operands must be valid UTF-8. An operand containing invalid UTF-8 bytes is rejected before the command runs, with exit status **2** and no changes to the repository. Human output reports the usage error on stderr; `--json` writes an error object with `kind: "usage"` on stdout, and `--markdown` writes `**Error:**` on stdout. The argument is never replaced with a similar Unicode filename.

## Options

| Option | Description |
| --- | --- |
| `<rev>` | Optional revision before `--` (same values as `--rev`). Defaults to `HEAD` when omitted. |
| `--rev <commit>` | Blame the file as it existed at this revision. Cannot be combined with a positional `<rev>`. |
| `-L RANGE` | Limit output with a Git-style line range in the final file (see below). Blame stops once every line in the range is attributed. |
| `--lines` | Same as `-L` (long form). |
| `--first-parent` | At merge commits, follow only the first parent when attributing lines. Default matches Git blame: all parents are considered. |
| `--group` | With `--json` only: emit grouped `hunks[]` instead of per-line `lines[]` (smaller on large files). Human and `--markdown` output stay per line. |
| `--ignore-rev <REV>` | Treat this commit as transparent while walking history (repeatable). Unknown revisions are an error. |
| `--ignore-revs-file <FILE>` | Read full-length commit oids to ignore, one per line (`#` comments, blank lines skipped). Repeatable; `""` clears entries loaded from config or earlier files. Missing files are an error. Relative paths are resolved from the repository root. Config `blame.ignoreRevsFile` (multi-valued) is applied before CLI files. |
| `-w`, `--ignore-all-space` | Ignore whitespace when comparing lines between revisions (same rules as [`grit diff -w`](../diff/)). Whitespace-only edits do not move blame to a newer commit. |
| `-C`, `--copy-detection` | Detect lines copied from other files (repeat up to three times for deeper search, matching `git blame -C` / `-C -C` / `-C -C -C`). A fourth `-C` is rejected with exit code 2. |

Human output prefixes ignored lines with `?` and lines that cannot be blamed past an ignored commit with `*` (same markers Git uses when `blame.markIgnoredLines` / `blame.markUnblamableLines` are enabled).

## Examples

Blame a file at `HEAD`:

```console
$ grit blame src/lib.rs
a1b2c3d (ada 2 days ago    1) use std::io;
f4e5d6c (bob 1 week ago    2) fn main() {
```

Blame an older revision and a slice of lines:

```console
$ grit blame v1.0 -- README.md
$ grit blame --rev=v1.0 -L 10,25 README.md
$ grit blame -L 10,+20 src/lib.rs
$ grit blame -L '/fn main/' src/main.rs
$ grit blame -L ':my_function' src/lib.rs
```

Scripting with JSON:

```console
$ grit blame --json story.txt
```

## JSON output

Schema: [v1](/schemas/blame/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `file` | string | Work-tree path that was blamed (repository-relative after cwd resolution). |
| `rev` | string | Full commit oid of the start revision. |
| `lines` | array | One record per line (after any `-L` filter), in final-line order. |
| `lines[].commit` | string | Full commit oid for the line. |
| `lines[].original_line` | number | 1-based line number in the originating commit. |
| `lines[].original_path` | string | Path of the line in its originating commit (matches `git blame --porcelain` `filename`, including after renames). |
| `lines[].final_line` | number | 1-based line number in the blamed revision. |
| `lines[].content` | string | Line text. |
| `lines[].ignored` | boolean | Present and `true` when the line was attributed through an ignored revision. |
| `lines[].unblamable` | boolean | Present and `true` when the line was added in an ignored revision and cannot be blamed further back. |
| `commits` | object | Map of commit oid → metadata for every line. |
| `commits.<oid>.author` | object | Author with `name`, `email`, and `date` (RFC 3339 — same shape as [`grit show`](../show/)). Author date is only here, not repeated on each line or hunk. |
| `commits.<oid>.summary` | string | First line of the commit message. |

### Grouped JSON (`--json --group`)

Same top-level `file`, `rev`, and `commits` map. Replaces `lines[]` with `hunks[]` (omit `lines`):

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `hunks[].commit` | string | Full commit oid for every line in the hunk. |
| `hunks[].original_path` | string | Originating path for the hunk (porcelain `filename`). |
| `hunks[].original_start` | number | 1-based first line in the originating commit. |
| `hunks[].final_start` | number | 1-based first line in the blamed revision. |
| `hunks[].count` | number | Number of consecutive final lines in the hunk. |
| `hunks[].ignored` | boolean | Present and `true` when the hunk passed through an ignored revision. |
| `hunks[].unblamable` | boolean | Present and `true` when blame could not continue past an ignored revision. |
| `hunks[].lines` | array of strings | Line text for each row in the hunk (same order as final lines). |

Hunk boundaries match `git blame --porcelain` group headers (`<oid> <orig> <final> <count>`). Prefer `--group` for agents and scripts on large files.

Example:

```json
{
  "file": "story.txt",
  "rev": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "lines": [
    {
      "commit": "919c45f33de5e5c0bd05f8ffb089f697f4644976",
      "original_line": 1,
      "original_path": "story.txt",
      "final_line": 1,
      "content": "alpha ONE"
    }
  ],
  "commits": {
    "919c45f33de5e5c0bd05f8ffb089f697f4644976": {
      "author": {
        "name": "Blame Test",
        "email": "blame@example.com",
        "date": "2024-03-09T12:26:40Z"
      },
      "summary": "edit first line"
    }
  }
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/blame/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.




```text
## Blame: `tracked.txt`

Revision `83352a61f6715ec3f0a297c6d6150725154fb833`

| Line | Mark | Commit | Author | Date | Original | Content |
| ---: | :---: | --- | --- | --- | ---: | --- |
| 1 |  | `83352a6` | Test | [TIME_AGO] | 1 | tracked v2 |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

- [`grit log`](log.md) — commit history
- [`grit show`](show.md) — one commit's message and file stats
