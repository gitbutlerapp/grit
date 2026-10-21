# Command page template

Every file in this directory (except this README) is one `grit` subcommand. The docs generator (`scripts/docs.py`) ignores this file.

## Library areas

Each core library area from the [project direction](../../ROADMAP.md) has at least one `grit` command. Use the library handle when embedding; use the CLI for day-to-day work.

| Library area | `grit` command(s) | Library handle |
| --- | --- | --- |
| Object database | [`cat`](cat.md), [`resolve`](resolve.md), [`repack`](repack.md), [`gc`](gc.md) | [`Repository`](rustdoc:grit_lib::repo::Repository), [`Objects`](rustdoc:grit_lib::objects::handle::Objects) |
| Network transport | [`fetch`](fetch.md), [`push`](push.md), [`remote refs`](remote.md), [`clone`](clone.md) | [`RepositoryRemotes`](rustdoc:grit_lib::repo_handles::RepositoryRemotes) |
| Refs | [`branch`](branch.md), [`tag`](tag.md), [`switch`](switch.md) | [`References`](rustdoc:grit_lib::refs::handle::References) |
| Index | [`add`](add.md), [`restore`](restore.md), [`status`](status.md) | [`IndexHandle`](rustdoc:grit_lib::index::handle::IndexHandle) |
| Stash | [`stash`](stash.md) | [`RepositoryStash`](rustdoc:grit_lib::repo_handles::RepositoryStash) |
| Reflog | [`reflog`](reflog.md) | [`RepositoryReflogs`](rustdoc:grit_lib::repo_handles::RepositoryReflogs) |
| Diffing | [`diff`](diff.md) | [`Diff`](rustdoc:grit_lib::diff::handle::Diff) |
| Revwalking | [`log`](log.md), [`revs`](revs.md), [`shortlog`](shortlog.md) | [`RevWalk`](rustdoc:grit_lib::revwalk::handle::RevWalk) |
| History rewrite | [`history`](history.md) | [`grit_lib::revwalk::history`](rustdoc:grit_lib::revwalk::history) |
| Revparsing | [`resolve`](resolve.md), [`merge-base`](merge-base.md), [`show`](show.md) | [`RevWalk`](rustdoc:grit_lib::revwalk::handle::RevWalk) |
| Config | [`config`](config.md) | [`ConfigHandle`](rustdoc:grit_lib::config::handle::ConfigHandle) |
| Ignore rules | [`ignore check`](ignore.md) | [`WorktreeRules`](rustdoc:grit_lib::worktree::worktree_rules::WorktreeRules) |
| Hooks | [`hooks`](hooks.md) | [`Repository::hooks`](rustdoc:grit_lib::repo::Repository) (via hook runner) |
| Patch IDs | [`patch-id`](patch-id.md) | [`compute_patch_id`](rustdoc:grit_lib::diff::patch_ids::compute_patch_id) |
| Notes | [`notes`](notes.md) | [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes) |
| Bundles | [`bundle`](bundle.md) | [`RepositoryBundles`](rustdoc:grit_lib::repo_handles::RepositoryBundles) |
| Fsck | [`fsck`](fsck.md) | [`repository_fsck`](rustdoc:grit_lib::maintenance::repository_fsck) |
| Worktrees | [`worktree`](worktree.md) | [`RepositoryWorktrees`](rustdoc:grit_lib::repo_handles::RepositoryWorktrees) |

Use this section order:

Every fenced code block must tag the opening fence with a language or format hint (bare ` ``` ` lines fail `load_site` validation):

| Tag | Use for |
| --- | --- |
| `text` | Synopsis usage lines and other plain prose or output without a shell prompt |
| `console` | Terminal sessions: a `$ grit …` line plus command output |
| `json` | Real `grit <cmd> --json` examples in the JSON output section |
| `bash` | Shell install/build commands without a `$` prompt |
| `rust`, `toml` | Library snippets and manifest fragments |

Closing fences stay untagged: ` ``` ` on its own line.

1. **Synopsis** — fenced usage lines (`text`).
2. **Description** — what the command does; `###` subsections are fine.
3. **Options** — table of flags and arguments (or a note pointing at global options).
4. **Examples** — human-readable terminal output from a real `grit` run.
5. **JSON output** — for commands that emit JSON with `--json`:
   - a **Schema:** line linking the committed v1 JSON Schema on the docs site (for example `Schema: [v1](/schemas/status/v1.json)`; use one link per subcommand schema when a page covers several);
   - at least one fenced ` ```json ` block containing real `grit <cmd> --json` output;
   - a **markdown field table** (`| Field | … |`) whose rows are **exactly** the top-level properties of that command’s schema(s)—no extra rows, no missing keys (CI compares against `content/schemas/*.v1.json`).
   **Exception:** plumbing commands (`manager`, `upload-pack`, `receive-pack`) and [`completions`](completions.md) do not use fenced JSON; their JSON output section explains that stdout carries the wire protocol, credential stream, or completion script instead. Every other command follows the schema link + fenced JSON + field-table rule.
6. **Markdown output** — **required** for every command that supports `--markdown` (including via global flags). Document real `--markdown` output, agent-oriented headings and bullets, and link the same v1 JSON Schema as the JSON output section so agents can rely on one contract.
7. **See also** — links to related command pages.

## Commit JSON object (`CommitJson`)

List and show commands share one commit object in `--json` output. The v1 schema defines it as **`CommitJson`** (see [`log`](../log/), [`show`](../show/), [`revs`](../revs/), [`shortlog`](../shortlog/)).

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `oid` | string | Full 40-character commit hex id. |
| `parents` | array of strings | Parent commit oids (empty for root commits). |
| `author` | object | Author `name`, `email`, and `date` (RFC 3339). |
| `committer` | object | Committer `name`, `email`, and `date` (RFC 3339). |
| `subject` | string | First line of the commit message. |
| `message` | string | Full message body with trailing whitespace trimmed. |
| `refs` | array | Ref decorations on this commit: each `{ "name", "kind" }` where `kind` is `branch`, `remote_branch`, `tag`, `stash`, `head`, `grafted`, or `other`. |
| `relative_date` | string or omitted | Human-relative author date (e.g. `3 days ago`) when the command supplies a clock. |
| `change_id` | string | Stable change id (reverse-hex). Stored in a `change-id` commit header when grit wrote the commit; otherwise derived deterministically from `oid`. |
| `change_id_stored` | boolean | `true` when `change_id` came from a `change-id` header in the commit object. |

List outcomes (`log`, `revs`, `shortlog`) also expose:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `truncated` | boolean | `true` when more commits matched the query than returned (`-n` or default page size). |
| `next` | string or null | Opaque continuation cursor for resumable lists (`log` only today), otherwise `null`. |

There is no generated HTML to commit: the site is built from these files on deploy. Preview with `make docs` and validate with `cd site && npm run check`.
