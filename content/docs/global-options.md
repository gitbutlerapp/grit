---
title: Global options
summary: Options that apply to every grit command.
---

These options work on any command (and on `grit` itself when you pass no subcommand).

| Option | Description |
| --- | --- |
| `--json` | Print one JSON object instead of human-readable text. Errors become `{"error": "..."}`. |
| `--filter <EXPR>` | Apply a jq expression to the JSON output. Requires `--json`. |
| `-h`, `--help` | Print help for `grit` or for a command. |
| `-V`, `--version` | Print the version of `grit`. |

Running `grit` with no command is the same as [`grit status`](../status/).
