# Command page template

Every file in this directory (except this README) is one `grit` subcommand. The docs generator (`scripts/docs.py`) ignores this file.

Use this section order:

1. **Synopsis** — fenced usage lines.
2. **Description** — what the command does; `###` subsections are fine.
3. **Options** — table of flags and arguments (or a note pointing at global options).
4. **Examples** — human-readable terminal output from a real `grit` run.
5. **JSON output** — for commands that emit JSON with `--json`:
   - at least one fenced ` ```json ` block containing real `grit <cmd> --json` output;
   - a **markdown field table** (`| Field | … |`) documenting every top-level key in each example (prose lists of keys are not enough).
   **Exception:** plumbing commands (`manager`, `upload-pack`, `receive-pack`) do not use fenced JSON; their JSON output section explains that stdout carries the credential or wire protocol instead. Every other command follows the fenced-JSON + field-table rule.
6. **Markdown output** — only when the command (or global flags) includes `--markdown`. Do not add this section until the flag exists.
7. **See also** — links to related command pages.

After editing, run `python3 scripts/docs.py` and commit the generated HTML under `docs/docs/`.
