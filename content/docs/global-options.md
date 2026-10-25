---
title: Global options
summary: Options that apply to every grit command.
---

These options work on any command (and on `grit` itself when you pass no subcommand).

| Option | Description |
| --- | --- |
| `--json` | Print one JSON object instead of human-readable text. Errors become `{"error": "..."}`. |
| `--markdown` | Print agent-friendly Markdown instead of human-readable text. Most commands render structured headings, lists, and tables (or fenced diffs) rather than dumping JSON. Mutually exclusive with `--json`. |
| `--filter <EXPR>` | Apply a jq expression to the JSON output. Requires `--json`. |
| `--threads <N>` | Worker threads for parallel library work inside this process. `0` selects all logical CPUs (Git-style auto); `1` forces serial paths. Does not change human, `--json`, or `--markdown` output shape. |
| `-h`, `--help` | Print help for `grit` or for a command. |
| `-v`, `--version` | Print the version of `grit`. `-V` works too. |

Running `grit` with no command is the same as [`grit status`](../status/).

## Repository and config options

These go **before** the command name (`grit -C repo status`, not `grit status -C repo`), as in Git, so they never collide with a command's own short flags.

| Option | Description |
| --- | --- |
| `-C <DIR>` | Run as if `grit` was started in `<DIR>`. Repeatable: each one is relative to the previous (`-C a -C b` is `a/b`). An empty `<DIR>` (`-C ""`) changes nothing. A directory that does not exist fails with exit `1`. |
| `-c <KEY=VALUE>` | Set a config value for this invocation only, without touching any config file. Repeatable; a later `-c` wins over an earlier one, over `GIT_CONFIG_COUNT` pairs and over every config file. A bare `<KEY>` with no `=` means `true`, and `<KEY>=` sets the empty string. An invalid key is a usage error (exit `2`). |
| `--git-dir <PATH>` | Use this repository directory instead of discovering one. Same as `GIT_DIR`. Without `--work-tree`, the current directory is the work tree (unless the repository is bare or sets `core.worktree`). |
| `--work-tree <PATH>` | Use this directory as the top of the work tree. Same as `GIT_WORK_TREE`. |

`-C` is applied first, so a relative `--git-dir` or `--work-tree` is resolved from the new directory. The overrides are exported to everything `grit` starts, using the same `GIT_DIR`, `GIT_WORK_TREE` and `GIT_CONFIG_PARAMETERS` encoding Git uses, so hooks that run `git` see the same repository and config.

```console
$ grit -C services/api status --json --filter .branch
"main"
$ grit -c user.name="CI Bot" -c user.email=ci@example.com commit -m "Update lockfile"
[main 3f9c2ab] Update lockfile
1 change committed
$ grit --git-dir /srv/site.git --work-tree /var/www/site status
```

Library embedders get the same behavior from [`Environment::with_git_dir`](rustdoc:grit_lib::config::environment::Environment), [`Environment::with_work_tree`](rustdoc:grit_lib::config::environment::Environment) and [`Environment::push_config_override`](rustdoc:grit_lib::config::environment::Environment) with a parsed [`ConfigOverride`](rustdoc:grit_lib::config::environment::ConfigOverride).

## Environment variables

The `grit` CLI snapshots the process environment into [`Environment`](../library/repository/) when opening a repository. Library embedders should construct an [`Environment`](../library/repository/) explicitly instead of relying on these variables.

| Variable | Effect |
| --- | --- |
| `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_PREFIX` | Repository discovery |
| `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES` | Index and object store paths. With `GIT_OBJECT_DIRECTORY` set, every command reads and writes objects there instead of `.git/objects`, which stays visible only when listed in `GIT_ALTERNATE_OBJECT_DIRECTORIES` |
| `GIT_CONFIG`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n`, `GIT_CONFIG_VALUE_n`, `GIT_CONFIG_PARAMETERS` | Config cascade |
| `HOME`, `XDG_CONFIG_HOME`, `USERPROFILE`, `USER`, `USERNAME` | Home and identity fallbacks |
| `GIT_AUTHOR_*`, `GIT_COMMITTER_*` | Author/committer name, email, and date overrides for new objects |
| `GIT_SSH`, `GIT_SSH_COMMAND` | SSH transport command (`GIT_SSH_COMMAND` overrides `core.sshCommand` and `GIT_SSH`; see Git precedence) |
| `GIT_HTTP_LOW_SPEED_LIMIT`, `GIT_HTTP_LOW_SPEED_TIME` | HTTP low-speed abort thresholds |
| `GIT_NOTES_REF`, `GIT_NOTES_DISPLAY_REF` | Notes ref selection |
| `GIT_ATTR_SOURCE`, `GIT_INDEX_VERSION`, `GIT_PRINT_SHA1_ELLIPSIS` | Attributes, index format, diff OID display |
| `GIT_NAMESPACE`, `GIT_REPLACE_REF_BASE`, `GIT_NO_REPLACE_OBJECTS` | Ref namespaces and replace refs |
| `GIT_TRACE`, `GIT_TRACE_SETUP`, `GIT_TRACE2_PERF` | Trace output |
| `GRIT_TRACE` | Grit operation spans to stderr (`1` / `true` / `yes`), or appended to a file when set to an absolute path; see [Debugging and tracing](../debugging/) |
| `PATH` | Helper lookup (e.g. signing programs) |
| `TZ` | Local timezone for dates when no explicit offset is given |
| `GIT_EXEC_PATH`, `GIT_INSTALL_ROOT` | Git helper discovery (credentials) |

Test-only knobs (`GIT_TEST_NO_WRITE_REV_INDEX`, `GIT_TEST_MIDX_WRITE_REV`, `GIT_TEST_REFTABLE_AUTOCOMPACTION`, `GIT_TEST_DATE_NOW`, and related values) are parsed into typed [`Environment`](../library/repository/) fields when present; they are not read elsewhere in `grit-lib`.
