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

There is no generated HTML to commit: the site is built from these files on deploy. Preview with `make docs` and validate with `cd site && npm run check`.
