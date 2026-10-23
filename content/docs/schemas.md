---
title: JSON schemas
summary: Versioned v1 JSON Schemas for every grit command's --json success object.
---

Every command that prints a JSON outcome object with `--json` has a versioned JSON Schema served at a stable URL (`https://grit-scm.com/schemas/<command>/vN.json`). Most commands are still on **v1**; `merge`, `pull`, and `pick` publish **v2** while **v1** remains frozen for older clients.

See the [Agent guide](../agents/#json-schema-versioning) for additive vs breaking rules, the CI backward-compatibility gate, and how to regenerate live files under `content/schemas/` with `GRIT_UPDATE_SCHEMAS=1`.

Plain command failures with `--json` validate against the shared [error schema](/schemas/error/v1.json).

## Command schemas

| Command path | Schema |
| --- | --- |
| `add` | [v1](/schemas/add/v1.json) |
| `auth` | [v1](/schemas/auth/v1.json) |
| `auth.logout` | [v1](/schemas/auth.logout/v1.json) |
| `blame` | [v1](/schemas/blame/v1.json) |
| `branch` | [v1](/schemas/branch/v1.json) |
| `bundle.create` | [v1](/schemas/bundle.create/v1.json) |
| `bundle.list` | [v1](/schemas/bundle.list/v1.json) |
| `bundle.verify` | [v1](/schemas/bundle.verify/v1.json) |
| `bundle.unbundle` | [v1](/schemas/bundle.unbundle/v1.json) |
| `clean` | [v1](/schemas/clean/v1.json) |
| `clone` | [v1](/schemas/clone/v1.json) |
| `commit` | [v1](/schemas/commit/v1.json) |
| `config` | [v1](/schemas/config/v1.json) |
| `diff` | [v1](/schemas/diff/v1.json) |
| `fetch` | [v1](/schemas/fetch/v1.json) |
| `init` | [v1](/schemas/init/v1.json) |
| `log` | [v1](/schemas/log/v1.json) |
| `merge` | [v2](/schemas/merge/v2.json), [v1](/schemas/merge/v1.json) |
| `mv` | [v1](/schemas/mv/v1.json) |
| `notes` | [v1](/schemas/notes/v1.json) |
| `notes.add` | [v1](/schemas/notes.add/v1.json) |
| `notes.list` | [v1](/schemas/notes.list/v1.json) |
| `notes.remove` | [v1](/schemas/notes.remove/v1.json) |
| `notes.show` | [v1](/schemas/notes.show/v1.json) |
| `pick` | [v1](/schemas/pick/v1.json) |
| `history.move` | [v1](/schemas/history.move/v1.json) |
| `history.reword` | [v1](/schemas/history.reword/v1.json) |
| `history.fixup` | [v1](/schemas/history.fixup/v1.json) |
| `history.absorb` | [v1](/schemas/history.absorb/v1.json) |
| `pull` | [v2](/schemas/pull/v2.json), [v1](/schemas/pull/v1.json) |
| `push` | [v1](/schemas/push/v1.json) |
| `remote` | [v1](/schemas/remote/v1.json) |
| `remote.add` | [v1](/schemas/remote.add/v1.json) |
| `remote.refs` | [v1](/schemas/remote.refs/v1.json) |
| `reflog` | [v1](/schemas/reflog/v1.json) |
| `reflog.delete` | [v1](/schemas/reflog.delete/v1.json) |
| `reflog.expire` | [v1](/schemas/reflog.expire/v1.json) |
| `reset` | [v1](/schemas/reset/v1.json) |
| `restore` | [v1](/schemas/restore/v1.json) |
| `revert` | [v1](/schemas/revert/v1.json) |
| `rm` | [v1](/schemas/rm/v1.json) |
| `shortlog` | [v1](/schemas/shortlog/v1.json) |
| `show` | [v1](/schemas/show/v1.json) |
| `skill` | [v1](/schemas/skill/v1.json) |
| `stash` | [v1](/schemas/stash/v1.json) |
| `stash.apply` | [v1](/schemas/stash.apply/v1.json) |
| `stash.drop` | [v1](/schemas/stash.drop/v1.json) |
| `stash.list` | [v1](/schemas/stash.list/v1.json) |
| `stash.pop` | [v1](/schemas/stash.pop/v1.json) |
| `stash.push` | [v1](/schemas/stash.push/v1.json) |
| `stash.show` | [v1](/schemas/stash.show/v1.json) |
| `status` | [v1](/schemas/status/v1.json) |
| `switch` | [v1](/schemas/switch/v1.json) |
| `tag` | [v1](/schemas/tag/v1.json) |
| `update` | [v1](/schemas/update/v1.json) |
| `worktree.add` | [v1](/schemas/worktree.add/v1.json) |
| `worktree.list` | [v1](/schemas/worktree.list/v1.json) |
| `worktree.prune` | [v1](/schemas/worktree.prune/v1.json) |
| `worktree.remove` | [v1](/schemas/worktree.remove/v1.json) |
| `worktree.lock` | [v1](/schemas/worktree.lock/v1.json) |
| `worktree.unlock` | [v1](/schemas/worktree.unlock/v1.json) |

Plumbing commands (`manager`, `upload-pack`, `receive-pack`) and shell completion generation do not emit JSON outcome objects and have no schemas here.

## See also

[Scripting with grit](../scripting/), [Agent guide](../agents/)
