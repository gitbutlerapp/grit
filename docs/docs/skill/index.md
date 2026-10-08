# grit skill

> Print an agent skill that explains how to use grit.

## Synopsis

```text
grit skill
```

## Description

Prints a `SKILL.md` file for coding agents that explains how to use `grit`: the JSON output contract, the everyday commands, how `grit commit` differs from `git commit`, how to stay non-interactive, and what to do with `git` instead. It uses the [Agent Skills](https://agentskills.io) format, with `name` and `description` front matter, so agents that load skills pick it up when a task involves version control.

The skill is built into the binary and names the version that printed it, so regenerate it after [`grit update`](https://grit-scm.com/docs/update/index.md) to keep it current.

Save it where your agent looks for skills, for example `.agents/skills/grit/SKILL.md` in a project, or `~/.claude/skills/grit/SKILL.md` for Claude Code across all projects.

## Options

`grit skill` takes no options besides the [global options](https://grit-scm.com/docs/global-options/index.md).

## Examples

Install the skill in the current project:

```console
$ mkdir -p .agents/skills/grit
$ grit skill > .agents/skills/grit/SKILL.md
```

Read the start of it:

```console
$ grit skill | head -4
---
name: grit
description: Use the grit CLI for version control in Git repositories — status, diffs, commits, branches, merges, cherry-picks, fetch/pull/push and tags — with JSON output for scripts and agents. Use when a task involves committing, branching, syncing with a remote or reading history and `grit` is installed.
---
```

## JSON output

Pass `--json` to get the skill text along with where to save it:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `name` | string | The skill's name, `grit`. |
| `version` | string | The grit version that generated the skill. |
| `path` | string | A conventional place to save it, relative to a project root. |
| `content` | string | The full `SKILL.md` text, front matter included. |

Example (with `content` shortened):

```json
{
  "name": "grit",
  "version": "0.5.2",
  "path": ".agents/skills/grit/SKILL.md",
  "content": "---\nname: grit\ndescription: Use the grit CLI for version control …"
}
```

## See also

[Agent guide](https://grit-scm.com/docs/agents/index.md), [grit update](https://grit-scm.com/docs/update/index.md), [Global options](https://grit-scm.com/docs/global-options/index.md)
