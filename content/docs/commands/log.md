---
title: grit log
summary: Show commits, newest first: the whole history in the pager on a terminal, a page at a time for scripts and agents.
group: History
order: 1
---

## Synopsis

```text
grit log [<REV>…] [-n <N>] [--cursor <token>] [--no-pager]
  [-p] [--stat | --numstat | --shortstat | --name-only | --name-status] [-U<N>]
  [--oneline] [--decorate] [--graph] [--graph-ascii] [--graph-unicode] [--group-by author] [--count]
  [--skip <N>] [--since|--after <date>] [--until|--before <date>]
  [--first-parent] [--merges | --no-merges] [--reverse]
  [--author <pat>] [--committer <pat>] [--grep <pat>]
  [-S <string>] [-G <pat>] [--pickaxe-regex]
  [-i] [--all-match] [--invert-grep] [-F] [--no-mailmap]
  [--all] [--branches[=<glob>]] [--tags[=<glob>]] [--remotes[=<glob>]]
  [--pathspec-from-file <file|->] [--pathspec-file-nul]
  [-- <path>…]
```

## Description

Lists commits reachable from the selected revisions (default `HEAD`), newest first. Each line shows the short commit id, the change id, the author, when the commit was made and its subject (the first line of its message). The author is the commit's author name when present, otherwise the part of their email address before the `@`.

How much it shows depends on who is reading:

- **On a terminal**, `grit log` shows the whole history, one line per commit, through the pager, and cuts every row to the terminal width so nothing wraps. Each row ends at the right edge with the lines the commit added and removed (`+12 -3`, against its first parent; merges and commits that change no lines show none). History is walked a batch at a time, so the first screen appears at once, and the walk stops when you quit the pager. `--no-pager` writes straight to the terminal. `-n` still limits the listing.
- **For scripts and agents** (`--json`, `--markdown`, or output piped or redirected), it shows a page of twenty commits (or `-n`). When there is more history, the last line gives the command for the next page, `→ more: grit log --cursor=<token>` (in Markdown, ``More history: run `grit log --cursor=<token>` ``), and `--json` puts the same token in `next`.

The pager is `GRIT_PAGER`, then the `core.pager` config, then `PAGER`, then `less`; an empty value or `cat` turns paging off. As in Git, `LESS=FRX` and `LV=-c` are set unless you set them, so a history that fits on one screen prints and exits.

On a [GitButler workspace](../status/#gitbutler-workspaces), `grit log` with no revision starts from the applied branches instead of `HEAD`, so GitButler's workspace commit is not listed. Naming a revision (`grit log HEAD`) walks exactly that.

Revision selection matches [`grit revs`](../revs/): plain tips, `A..B`, `A...B`, `^tip`, and the same ref-set flags (`--all`, `--branches`, and so on). Path operands after `--` limit to commits that touch those paths (same cwd rules as [`grit diff`](../diff/)). Attribute pathspecs such as `:(attr:myattr)` use the repository's current `.gitattributes` stack, like diff and status; they do not read historical attribute files from each commit's tree. When a path filter matches no commits on a repository that already has history, human output is blank (not "No commits yet."). Path filters use the same author/date output as the default log, not the bare oid list from `grit revs`. Without `-n`, every matching commit is listed when paths are set (not capped at a page).

The continuation token is short and local to the repository (`lw1.s.` plus eight characters, stored under `.git/grit/log-walk/`; in a read-only repository it is the longer self-contained `lw1.` form instead); it encodes the revision selection, so the next page needs only `--cursor=<token>`. Path filters, commit filters (`--author`, `--grep`, …), pickaxe filters (`-S`, `-G`), `--reverse`, `--first-parent`, merge filters, `--skip`, and a one-shot `-n` without `--cursor` do not emit a continuation cursor; when such a listing stops early, the hint is ``→ more: rerun with a larger `-n` ``.

Pickaxe filters match Git's `-S` and `-G`: `-S` keeps commits where the number of occurrences of a string changed in the diff against the first parent (literal string unless `--pickaxe-regex`); `-G` keeps commits where added or removed lines match a BRE. `-i` applies to both. Binary files are skipped for `-G` (not for `-S`). `-S` and `-G` cannot be combined.

As in Git, the diff is limited to the path operands first, then deleted and added files are paired as renames (`diff.renames`, `diff.renameLimit`), and only then is each file tested. A rename whose old and new file hold the same number of matches (`-S`), or whose diff changes no matching line (`-G`), is not listed. A file moved into the path operands from outside them counts as added, so `grit log -S needle -- new/path` still lists the move. With `diff.renames=false` every rename is a deletion plus an addition. Merges are skipped, except under `--first-parent`, where they are diffed against their first parent.

Author, committer, and message filters match [`grit revs`](../revs/): patterns use Git's default POSIX BRE rules unless `-F` / `--fixed-strings` is set; multiple `--author` or `--committer` patterns OR together; multiple `--grep` patterns OR together unless `--all-match`; `--invert-grep` negates only the grep group; `-i` applies to all. When `log.mailmap` is unset or true (and `--no-mailmap` is not passed), `.mailmap` is applied before matching author or committer (same sources as Git). Invalid patterns exit **129** with `kind: "invalid_pattern"` in `--json` output.

Diff detail flags (same family as [`grit diff`](../diff/)) append output under each commit line: `-p` / `--patch` (unified patch), `--stat`, `--numstat`, `--shortstat`, `--name-only`, or `--name-status`. Only one format at a time. `-U` sets unified context lines for patches (default from `diff.context`).

With `--stat`, each commit is also summarized in JSON under `stat` (legacy shape matching [`grit show`](../show/)). Merge commits show the change against their first parent for stat and name modes; `-p` skips merge patches (like `git log -p`). Path operands limit both which commits appear and the paths included in each patch.

With `--graph`, human output draws a commit graph in the left margin (Unicode box-drawing by default, or ASCII `*|` with `--graph-ascii`). `--graph-unicode` is accepted as an alias for the default Unicode graph and implies `--graph`. Graph mode uses topological ordering like Git's `log --graph`, including with path and author/committer/grep filters (filtered commits are skipped while keeping lane connectivity). `--json` rejects `--graph` (exit **1**, `kind: "incompatible_flags"`) because graph columns are human-only; `--markdown` lists commits without graph columns and includes a note that `--graph` is human-only. `--graph` cannot be combined with diff format flags, `--group-by`, or `--cursor`.

## Options

| Option | Description |
| --- | --- |
| `<REV>…` | Tips and ranges to walk (default `HEAD`). |
| `-n`, `--limit <N>` | Stop after `N` commits (on a terminal too). On the default walk, this is the page size when continuing with `--cursor`. With path filters, caps how many matching commits are returned; omit it to list all matches. A one-shot `-n` without `--cursor` and without paths lists `N` commits with no `next` cursor. `--max-count` is Git’s well-known alias. Git’s `-1` / `-3` shorthand (for example `grit log -1` or `grit log --graph --oneline -3`) is accepted as `-n`. |
| `--cursor <token>` | Continue a paged walk using the token from the `→ more` line or `next` from `--json` (short `lw1.s.` tokens are stored under `.git/grit/log-walk/`; long `lw1.` base64 tokens still work). An empty value is ignored. The cursor encodes the original revision selection, date cutoffs, and walk queue—when resuming, pass `--cursor=<token>` alone (optional `-n` for page size). Revision operands, date filters, and ref-set flags are rejected when continuing. Cannot be combined with path operands. |
| `--before <date>` | Alias for `--until` (date cutoff on committer date). |
| `--skip <N>` | Skip `N` commits after ordering (cursor walks do not support `--skip`). |
| `--since`, `--after <date>` | Exclude commits older than this date (committer date). Accepts Git approxidate strings (for example `2024-01-01`, `1 week ago`, `yesterday`) as well as ISO/RFC forms. |
| `--until`, `--before <date>` | Exclude commits newer than this date. |
| `--first-parent` | Follow only the first parent at merges (disables cursor paging). |
| `--merges` | Only merge commits (conflicts with `--no-merges`; disables cursor paging). |
| `--no-merges` | Exclude merge commits (conflicts with `--merges`; disables cursor paging). |
| `--reverse` | Print the selected window oldest-first. `-n`, `--skip`, and date filters choose the window (newest `N` commits after skip); `--reverse` only flips that window (disables cursor paging). |
| `--all` | Include every ref tip. |
| `--branches[=<glob>]` | Branch tips, optionally filtered by glob under `refs/heads/`. |
| `--tags[=<glob>]` | Tag tips, optionally filtered by glob under `refs/tags/`. |
| `--remotes[=<glob>]` | Remote-tracking tips, optionally filtered by glob under `refs/remotes/`. |
| `-p`, `--patch` | Unified patch under each commit (merge commits omit the patch). |
| `--patch-text` | Git-apply-compatible unified patch per commit (`git diff --binary` shape). Honors trailing pathspecs. Paged walks print the `→ more` continuation after the final patch. Merge commits omit the patch body (same as `-p`). Human output only; not combinable with `--json` or `--markdown`. |
| `-W`, `--function-context` | Expand patch hunks to enclosing function boundaries (same as [`grit diff`](diff.md)). |
| `--stat` | Per-file change bars and totals under each commit. The JSON `stat` summary uses the same path selection and counts as `files` and `totals`; it does not compute patch hunks. |
| `--numstat` | Tab-separated insertion/deletion/path lines per commit. |
| `--shortstat` | One summary line per commit. |
| `--name-only` | Changed paths only, one per line per commit. |
| `--name-status` | Status letter and path(s) per line per commit. |
| `-U`, `--unified <N>` | Context lines for `-p` (Git `-U`). |
| `-W`, `--function-context` | Expand patch hunks to enclosing function boundaries when `-p` is set. |
| `--oneline` | One line per commit: short oid and subject (`--json` unchanged). With `--decorate`, refs appear in parentheses after the oid. |
| `--graph` | Draw a commit graph beside each commit in human output only (implies topological order; Unicode lanes by default). |
| `--no-pager` | On a terminal, write straight to it instead of through the pager. |
| `--graph-ascii` | Use ASCII `*|` lane characters in `--graph` output instead of Unicode. |
| `--graph-unicode` | Same as `--graph` with Unicode lanes (default); implies `--graph`. Conflicts with `--graph-ascii`. |
| `--decorate` | Show branch/tag/`HEAD` labels on each commit in human and Markdown output (refs are always in `--json`). |
| `--group-by author` | Aggregate by author (mailmap-aware, sorted like `git shortlog -sne`). Uses the rev walk for the full selection (no cursor paging). |
| `--count` | With `--group-by`, omit per-commit lists in `--json` (counts and author names only). |
| `--author <pat>` | Only commits whose author matches (repeatable; OR). |
| `--committer <pat>` | Only commits whose committer matches (repeatable; OR). |
| `--grep <pat>` | Only commits whose message matches (repeatable; OR unless `--all-match`). |
| `-S <string>` | Only commits where this string's occurrence count changed in the tree diff vs the first parent. |
| `-G <pat>` | Only commits where added/removed lines match this BRE (binary files skipped). |
| `--pickaxe-regex` | Treat `-S` as a BRE instead of a literal string (not valid with `-G`). |
| `-i`, `--regexp-ignore-case` | Case-insensitive patterns for author, committer, grep, and pickaxe. |
| `--all-match` | Every `--grep` pattern must match. |
| `--invert-grep` | Exclude commits that match the grep group. |
| `-F`, `--fixed-strings` | Literal string patterns (no regex). |
| `--no-mailmap` | Do not apply `.mailmap` for author/committer matching. |
| `--pathspec-from-file <file\|->` | Read pathspecs from a file (`-` for stdin), in addition to argv paths after `--`. |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated. |
| `<path>…` | After `--`, limit to commits that touch these paths (cwd-relative under the work tree, like `grit diff`; all matches unless `-n` is set). |

## Examples

Browse the whole history on a terminal (in the pager):

```console
$ grit log
  b52cca6  wuwxlkvx  Ada Lovelace  2 minutes ago  Handle empty input     +9  -3
  5fbefea  nrntqslp  Ada Lovelace  1 hour ago     Add a greeting test   +24  -0
  ...
  2efcc2d  krnxwkpm  Ada Lovelace  3 days ago     Start the project     +41  -0
```

From a script or an agent, the same command shows a page and how to get the next one:

```console
$ grit log | cat
  b52cca6  wuwxlkvx  Ada Lovelace  2 minutes ago  Handle empty input
  5fbefea  nrntqslp  Ada Lovelace  1 hour ago     Add a greeting test
  ...

→ more: grit log --cursor=lw1.s.a1b2c3d4
```

Show the next page:

```console
$ grit log --cursor=lw1.s.a1b2c3d4
```

See which files each recent commit touched:

```console
$ grit log --stat
  b52cca6  ada  2 minutes ago  Handle empty input

 src/parse.rs | 12 +++++++++---
 1 file changed, 9 insertions(+), 3 deletions(-)

  5fbefea  ada  1 hour ago     Add a greeting test
  ...
```

Get the subjects of the last twenty commits:

```console
$ grit log --json --filter '.commits[].subject'
```

Show the five newest commits:

```console
$ grit log -n 5
```

History of one file:

```console
$ grit log -- src/lib.rs
```

Commits on a range:

```console
$ grit log -n 20 main..feature
```

Authors and dates:

```console
$ grit log --author=ada --since='2 weeks ago' -n 20
```

## JSON output

Schema: [v1](/schemas/log/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Up to twenty commits by default, or up to `-n` when set, newest first. Each entry uses the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape (`oid`, `parents`, `author`, `committer`, `subject`, `message`, `refs`, `change_id`, `change_id_stored`, optional `relative_date`). Defaults to `[]` when omitted (for example with `--group-by author`, which fills `groups` instead). |
| `commits[].files` | array | With any diff detail flag: same [`FileDiff`](/schemas/diff/v1.json) objects as [`grit diff`](../diff/) (including `stat` sub-objects for `--stat` / `--numstat`). |
| `commits[].totals` | object | With stat-style flags: `files_changed`, `insertions`, `deletions`. |
| `commits[].stat` | object | Only with `--stat`: duplicate summary in the legacy [`grit show`](../show/) `stat` shape (`files`, `files_changed`, …). |
| `groups` | array | With `--group-by author`: `{name, email, count, commits?}` sorted like `git shortlog -sne`. `commits` holds full [`CommitJson`](../README.md#commit-json-object-commitjson) rows unless `--count` is set. |
| `next` | string or null | Opaque continuation cursor (prefix `lw1.`) to pass to `--cursor` for the next page, or `null` when there is no more history or when the walk does not support paging (paths, `--reverse`, `--group-by`, and similar). |
| `truncated` | boolean | **Required.** `true` when more commits matched the query than returned in this page (`-n` or the default page size). |

Combining `--json` with `--graph` fails with exit **1** and `{"error":"…","kind":"incompatible_flags"}` on stdout (same message as human mode).

Example:

```json
{
  "commits": [
    {
      "oid": "83352a61f6715ec3f0a297c6d6150725154fb833",
      "parents": [
        "062ca6e74f5603f4d09063c0f46cd07d8c96e019"
      ],
      "author": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "committer": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "subject": "second",
      "message": "second",
      "refs": [
        {
          "name": "HEAD",
          "kind": "head"
        },
        {
          "name": "main",
          "kind": "branch"
        }
      ],
      "relative_date": "[TIME_AGO]",
      "change_id": "xzyqzyyzswyupvtlsovyzultryuqzlvk",
      "change_id_stored": true
    },
    {
      "oid": "062ca6e74f5603f4d09063c0f46cd07d8c96e019",
      "parents": [],
      "author": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "committer": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "subject": "first",
      "message": "first",
      "refs": [
        {
          "name": "feature",
          "kind": "branch"
        }
      ],
      "relative_date": "[TIME_AGO]",
      "change_id": "prkwwuwrytvxrwozplzrxmvozwutplkq",
      "change_id_stored": true
    }
  ],
  "next": null,
  "truncated": false
}
```

## Markdown output

Schema: [v1](/schemas/log/v1.json)
Pass [`--markdown`](../global-options/) for a bullet list of commits (short oid, subject, author, relative date):

```text
- `b52cca6` Handle empty input (Ada Lovelace, 2 minutes ago)
- `5fbefea` Add a greeting test (Ada Lovelace, 1 hour ago)

More history: run `grit log --cursor=lw1.s.a1b2c3d4`
```


When the current branch has no commits yet (for example right after [`grit init`](init.md)):

```console
$ grit log
error: no commits yet on branch main — make one with `grit commit "message"`
hint: make one with `grit commit "message"`
```

Structured `--json` failures use `kind` `branch_no_commits` (see [Exit codes](../exit-codes/)).

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit revs](../revs/), [grit shortlog](../shortlog/), [grit show](../show/)