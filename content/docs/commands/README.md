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
   - at least one fenced ` ```json ` block containing real `grit <cmd> --json` output;
   - a **markdown field table** (`| Field | … |`) documenting every top-level key in each example (prose lists of keys are not enough).
   **Exception:** plumbing commands (`manager`, `upload-pack`, `receive-pack`) and [`completions`](completions.md) do not use fenced JSON; their JSON output section explains that stdout carries the wire protocol, credential stream, or completion script instead. Every other command follows the fenced-JSON + field-table rule.
6. **Markdown output** — only when the command (or global flags) includes `--markdown`. Do not add this section until the flag exists.
7. **See also** — links to related command pages.

There is no generated HTML to commit: the site is built from these files on deploy. Preview with `make docs` and validate with `cd site && npm run check`.
