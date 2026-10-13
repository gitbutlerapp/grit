# Command page template

Every file in this directory (except this README) is one `grit` subcommand. The docs generator (`scripts/docs.py`) ignores this file.

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
