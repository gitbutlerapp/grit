# Global options

> Options that apply to every grit command.

These options work on any command (and on `grit` itself when you pass no subcommand).

| Option | Description |
| --- | --- |
| `--json` | Print one JSON object instead of human-readable text. Errors become `{"error": "..."}`. |
| `--markdown` | Print agent-friendly Markdown instead of human-readable text. Most commands render structured headings, lists, and tables (or fenced diffs) rather than dumping JSON. Mutually exclusive with `--json`. |
| `--filter <EXPR>` | Apply a jq expression to the JSON output. Requires `--json`. |
| `-h`, `--help` | Print help for `grit` or for a command. |
| `-V`, `--version` | Print the version of `grit`. |

Running `grit` with no command is the same as [`grit status`](https://grit-scm.com/docs/status/index.md).

## Environment variables

The `grit` CLI snapshots the process environment into [`Environment`](https://grit-scm.com/docs/library/repository/index.md) when opening a repository. Library embedders should construct an [`Environment`](https://grit-scm.com/docs/library/repository/index.md) explicitly instead of relying on these variables.

| Variable | Effect |
| --- | --- |
| `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_PREFIX` | Repository discovery |
| `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES` | Index and object store paths |
| `GIT_CONFIG`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n`, `GIT_CONFIG_VALUE_n`, `GIT_CONFIG_PARAMETERS` | Config cascade |
| `HOME`, `XDG_CONFIG_HOME`, `USERPROFILE`, `USER`, `USERNAME` | Home and identity fallbacks |
| `GIT_AUTHOR_*`, `GIT_COMMITTER_*` | Author/committer name, email, and date overrides for new objects |
| `GIT_SSH`, `GIT_SSH_COMMAND` | SSH transport command |
| `GIT_HTTP_LOW_SPEED_LIMIT`, `GIT_HTTP_LOW_SPEED_TIME` | HTTP low-speed abort thresholds |
| `GIT_NOTES_REF`, `GIT_NOTES_DISPLAY_REF` | Notes ref selection |
| `GIT_ATTR_SOURCE`, `GIT_INDEX_VERSION`, `GIT_PRINT_SHA1_ELLIPSIS` | Attributes, index format, diff OID display |
| `GIT_NAMESPACE`, `GIT_REPLACE_REF_BASE`, `GIT_NO_REPLACE_OBJECTS` | Ref namespaces and replace refs |
| `GIT_TRACE`, `GIT_TRACE_SETUP`, `GIT_TRACE2_PERF` | Trace output |
| `PATH` | Helper lookup (e.g. signing programs) |
| `TZ` | Local timezone for dates when no explicit offset is given |
| `GIT_EXEC_PATH`, `GIT_INSTALL_ROOT` | Git helper discovery (credentials) |

Test-only knobs (`GIT_TEST_NO_WRITE_REV_INDEX`, `GIT_TEST_MIDX_WRITE_REV`, `GIT_TEST_REFTABLE_AUTOCOMPACTION`, `GIT_TEST_DATE_NOW`, and related values) are parsed into typed [`Environment`](https://grit-scm.com/docs/library/repository/index.md) fields when present; they are not read elsewhere in `grit-lib`.
